//! Where the chain looks: the documented locations of spec §2, and the order a
//! directory variable wins over the home. Every path is a scratch one.

mod support;

use p1_auth::{CredentialSpec, Locations, SourceName, describe};
use support::{Scratch, claude_login, login};

const ROUTE: &str = "test-route";

fn api_key() -> CredentialSpec {
    serde_json::from_str(
        r#"{"kind":"api-key","env":"P1_AUTH_TEST_KEY","borrow":["opencode:opencode-go","pi:opencode-go"]}"#,
    )
    .unwrap()
}

fn claude_oauth() -> CredentialSpec {
    serde_json::from_str(r#"{"kind":"claude-code-oauth"}"#).unwrap()
}

fn codex_oauth() -> CredentialSpec {
    serde_json::from_str(r#"{"kind":"codex-oauth"}"#).unwrap()
}

fn chosen(spec: &CredentialSpec, locations: &Locations) -> Option<SourceName> {
    describe(ROUTE, spec, locations).chosen
}

#[test]
fn the_home_locations_are_the_documented_ones() {
    let scratch = Scratch::new();
    let locations = scratch.locations();

    scratch.write(
        ".local/share/opencode/auth.json",
        &login("opencode", "opencode-go", "K"),
    );
    assert_eq!(
        chosen(&api_key(), &locations),
        Some(SourceName::OpencodeLogin)
    );

    scratch.write(".pi/agent/auth.json", &login("pi", "opencode-go", "K"));
    scratch.write(
        ".local/share/opencode/auth.json",
        &login("opencode", "some-other", "K"),
    );
    assert_eq!(chosen(&api_key(), &locations), Some(SourceName::PiLogin));

    scratch.write(
        ".claude/.credentials.json",
        &claude_login("A", "R", 4_102_444_800_000),
    );
    assert_eq!(
        chosen(&claude_oauth(), &locations),
        Some(SourceName::ClaudeCodeLogin)
    );

    scratch.write(
        ".codex/auth.json",
        &serde_json::json!({ "tokens": { "access_token": "A" } }).to_string(),
    );
    assert_eq!(
        chosen(&codex_oauth(), &locations),
        Some(SourceName::CodexLogin)
    );
}

#[test]
fn the_store_lives_under_the_config_home() {
    let scratch = Scratch::new();
    scratch.write(
        ".config/p1/auth.json",
        &serde_json::json!({ ROUTE: { "type": "api_key", "key": "K" } }).to_string(),
    );
    assert_eq!(
        chosen(&api_key(), &scratch.locations()),
        Some(SourceName::P1Store)
    );
}

#[test]
fn a_directory_variable_wins_over_the_home() {
    // Each case puts an UNUSABLE entry in the home location: if the home were read
    // anyway, the source would be unusable and no source would be chosen.
    let broken_opencode = r#"{"opencode-go":{"type":"api","key":"!FAKE-COMMAND"}}"#;
    let broken_claude = "FAKE-NOT-JSON";
    let broken_codex = "FAKE-NOT-JSON";

    let data = Scratch::new();
    data.write(
        "opencode/auth.json",
        &login("opencode", "opencode-go", "FAKE-DATA"),
    );
    let scratch = Scratch::new();
    scratch.write(".local/share/opencode/auth.json", broken_opencode);
    let locations = scratch.locations().with_xdg_data_home(Some(data.home()));
    assert_eq!(
        chosen(&api_key(), &locations),
        Some(SourceName::OpencodeLogin)
    );

    let pi_dir = Scratch::new();
    pi_dir.write("auth.json", &login("pi", "opencode-go", "FAKE-PI"));
    let scratch = Scratch::new();
    scratch.write(".pi/agent/auth.json", broken_opencode);
    let locations = scratch.locations().with_pi_agent_dir(Some(pi_dir.home()));
    assert_eq!(chosen(&api_key(), &locations), Some(SourceName::PiLogin));

    let claude_dir = Scratch::new();
    claude_dir.write(
        ".credentials.json",
        &claude_login("A", "R", 4_102_444_800_000),
    );
    let scratch = Scratch::new();
    scratch.write(".claude/.credentials.json", broken_claude);
    let locations = scratch.locations().with_env_lookup({
        let dir = claude_dir.home();
        move |name| (name == "CLAUDE_CONFIG_DIR").then(|| dir.display().to_string())
    });
    assert_eq!(
        chosen(&claude_oauth(), &locations),
        Some(SourceName::ClaudeCodeLogin)
    );

    let codex_dir = Scratch::new();
    codex_dir.write(
        "auth.json",
        &serde_json::json!({ "tokens": { "access_token": "A" } }).to_string(),
    );
    let scratch = Scratch::new();
    scratch.write(".codex/auth.json", broken_codex);
    let locations = scratch.locations().with_env_lookup({
        let dir = codex_dir.home();
        move |name| (name == "CODEX_HOME").then(|| dir.display().to_string())
    });
    assert_eq!(
        chosen(&codex_oauth(), &locations),
        Some(SourceName::CodexLogin)
    );
}

#[test]
fn a_blank_directory_variable_counts_as_unset() {
    let scratch = Scratch::new();
    scratch.write(
        ".claude/.credentials.json",
        &claude_login("A", "R", 4_102_444_800_000),
    );
    let locations = scratch
        .locations()
        .with_env_lookup(|name| (name == "CLAUDE_CONFIG_DIR").then(String::new));
    assert_eq!(
        chosen(&claude_oauth(), &locations),
        Some(SourceName::ClaudeCodeLogin),
        "a blank override must not hide the home's login"
    );
}
