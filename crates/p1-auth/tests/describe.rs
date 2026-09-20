//! Must-pass (e) of the spec: `describe` names the source a route would use, for
//! every row of the precedence table — and never contains a value.

mod support;

use p1_auth::{CredentialSpec, Presence, SourceName, describe};
use support::{FakeEnv, NEVER_EXPIRES_MS, Scratch, claude_login, login};

const SENTINEL: &str = "FAKE-SENTINEL-4a9c";
const STORE: &str = ".config/p1/auth.json";
const OPENCODE: &str = ".local/share/opencode/auth.json";
const PI: &str = ".pi/agent/auth.json";
const CLAUDE: &str = ".claude/.credentials.json";
const CODEX: &str = ".codex/auth.json";

fn spec(json: &str) -> CredentialSpec {
    serde_json::from_str(json).expect("the `[credential]` table parses")
}

fn api_key() -> CredentialSpec {
    spec(
        r#"{"kind":"api-key","env":"P1_AUTH_TEST_KEY",
            "borrow":["opencode:opencode-go","pi:opencode-go"]}"#,
    )
}

fn claude_oauth() -> CredentialSpec {
    spec(r#"{"kind":"claude-code-oauth"}"#)
}

fn codex_oauth() -> CredentialSpec {
    spec(r#"{"kind":"codex-oauth"}"#)
}

fn chosen(spec: &CredentialSpec, scratch: &Scratch, env: &FakeEnv) -> Option<SourceName> {
    describe("test-route", spec, &env.locations(scratch)).chosen
}

/// Every row of must-pass (a), as the report names it.
#[test]
fn describe_names_the_chosen_source_for_every_row_of_the_precedence() {
    let scratch = Scratch::new();
    let env = FakeEnv::new();
    let store = serde_json::json!({
        "test-route": { "type": "api_key", "key": "FAKE-STORE" },
        "zz-oauth-route": {
            "type": "oauth", "access": "FAKE-STORE-ACCESS", "refresh": "FAKE-R",
            "expires": NEVER_EXPIRES_MS,
        },
    })
    .to_string();
    scratch.write(STORE, &store);
    scratch.write(OPENCODE, &login("opencode", "opencode-go", "FAKE-OPENCODE"));
    scratch.write(PI, &login("pi", "opencode-go", "FAKE-PI"));
    scratch.write(
        CLAUDE,
        &claude_login("FAKE-CLAUDE", "FAKE-R", NEVER_EXPIRES_MS),
    );
    scratch.write(
        CODEX,
        &serde_json::json!({ "tokens": { "access_token": "FAKE-CODEX" } }).to_string(),
    );

    env.set("P1_AUTH_TEST_KEY", "FAKE-ENV");
    assert_eq!(
        chosen(&api_key(), &scratch, &env),
        Some(SourceName::Env("P1_AUTH_TEST_KEY".to_string()))
    );

    env.clear("P1_AUTH_TEST_KEY");
    assert_eq!(
        chosen(&api_key(), &scratch, &env),
        Some(SourceName::P1Store)
    );

    scratch.write(
        STORE,
        &serde_json::json!({ "other-route": { "type": "api_key", "key": "FAKE-OTHER" } })
            .to_string(),
    );
    assert_eq!(
        chosen(&api_key(), &scratch, &env),
        Some(SourceName::OpencodeLogin)
    );

    scratch.write(OPENCODE, &login("opencode", "some-other-key", "FAKE-OTHER"));
    assert_eq!(
        chosen(&api_key(), &scratch, &env),
        Some(SourceName::PiLogin)
    );

    // The two OAuth rows: env, then the store, then the CLI's own login file.
    assert_eq!(
        chosen(&claude_oauth(), &scratch, &env),
        Some(SourceName::ClaudeCodeLogin)
    );
    assert_eq!(
        chosen(&codex_oauth(), &scratch, &env),
        Some(SourceName::CodexLogin)
    );

    let empty = Scratch::new();
    assert_eq!(chosen(&claude_oauth(), &empty, &env), None);
}

/// Every source holds the sentinel, one at a time, and no report may show it —
/// in `Debug`, in `Display` or in the line `p1 env show` prints.
#[test]
fn describe_never_contains_a_value() {
    let scratch = Scratch::new();
    let env = FakeEnv::new();

    // The environment variable.
    env.set("P1_AUTH_TEST_KEY", SENTINEL);
    // The store, for the api-key route and for the oauth route.
    scratch.write(
        STORE,
        &serde_json::json!({
            "test-route": { "type": "api_key", "key": SENTINEL },
            "oauth-route": {
                "type": "oauth", "access": SENTINEL, "refresh": SENTINEL,
                "expires": NEVER_EXPIRES_MS, "account_id": SENTINEL,
            },
        })
        .to_string(),
    );
    // The borrowed logins, and the two CLIs' own files.
    scratch.write(OPENCODE, &login("opencode", "opencode-go", SENTINEL));
    scratch.write(PI, &login("pi", "opencode-go", SENTINEL));
    scratch.write(CLAUDE, &claude_login(SENTINEL, SENTINEL, NEVER_EXPIRES_MS));
    scratch.write(
        CODEX,
        &serde_json::json!({ "tokens": { "access_token": SENTINEL, "refresh_token": SENTINEL } })
            .to_string(),
    );

    let mut reports = vec![
        describe("test-route", &api_key(), &env.locations(&scratch)),
        describe("oauth-route", &claude_oauth(), &env.locations(&scratch)),
        describe("oauth-route", &codex_oauth(), &env.locations(&scratch)),
    ];
    // And a source that exists but is unusable: the reason names no value either.
    scratch.write(
        OPENCODE,
        &serde_json::json!({ "opencode-go": { "type": "api", "key": format!("!{SENTINEL}") } })
            .to_string(),
    );
    env.clear("P1_AUTH_TEST_KEY");
    reports.push(describe("test-route", &api_key(), &env.locations(&scratch)));

    for report in &reports {
        let rendered = format!("{report:?} {} {}", report.line(), report.chosen.is_some());
        let presence = report
            .tried
            .iter()
            .map(|(_, presence)| presence.to_string())
            .collect::<String>();
        for text in [rendered, presence] {
            assert!(!text.contains(SENTINEL), "a report leaked a value: {text}");
        }
    }
    assert!(
        reports[3]
            .tried
            .iter()
            .any(|(_, presence)| matches!(presence, Presence::Unusable(_))),
        "the unusable source is reported as such: {:?}",
        reports[3]
    );
}

/// The line `p1 env show` prints: the chosen source, or what to do instead.
#[test]
fn the_env_show_line_names_the_source_or_what_to_do() {
    let scratch = Scratch::new();
    let env = FakeEnv::new();
    scratch.write(OPENCODE, &login("opencode", "opencode-go", "FAKE-OPENCODE"));

    let line = describe("test-route", &api_key(), &env.locations(&scratch)).line();
    assert_eq!(line, "opencode login");

    env.set("P1_AUTH_TEST_KEY", "FAKE-ENV");
    let line = describe("test-route", &api_key(), &env.locations(&scratch)).line();
    assert_eq!(line, "env P1_AUTH_TEST_KEY");

    let empty = Scratch::new();
    let line = describe("test-route", &api_key(), &empty.locations()).line();
    assert!(line.starts_with("none — "), "{line}");
    assert!(
        line.contains("P1_AUTH_TEST_KEY") && line.contains("opencode"),
        "the line says what to do: {line}"
    );
}
