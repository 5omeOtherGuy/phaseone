//! The `[credential]` table itself (spec §2): the file format is unchanged, and
//! the one thing this step added is that an `api-key` route must NAME the variable
//! that holds its key.

use p1_auth::{BorrowSource, CredentialKind, CredentialSpec, Locations};

fn spec(json: &str) -> Result<CredentialSpec, serde_json::Error> {
    serde_json::from_str(json)
}

#[test]
fn the_three_kinds_parse_by_their_route_file_spelling() {
    for (text, kind, name) in [
        (
            r#"{"kind":"api-key","env":"A_KEY"}"#,
            CredentialKind::API_KEY,
            "api-key",
        ),
        (
            r#"{"kind":"claude-code-oauth"}"#,
            CredentialKind::module("claude-code-oauth"),
            "claude-code-oauth",
        ),
        (
            r#"{"kind":"codex-oauth"}"#,
            CredentialKind::module("codex-oauth"),
            "codex-oauth",
        ),
    ] {
        let parsed = spec(text).expect("the table parses");
        assert_eq!(parsed.kind, kind);
        assert_eq!(parsed.kind.name(), name);
        assert!(parsed.borrow.is_empty());
    }
}

/// Issue #134: a fourth kind, `none`, on its own test so the three above stay the
/// sentence they were (ADR-0029: a pre-existing acceptance test is not renamed).
#[test]
fn the_none_kind_parses_and_is_labelled_as_proxy_injected() {
    let parsed = spec(r#"{"kind":"none"}"#).expect("the table parses");
    assert_eq!(parsed.kind, CredentialKind::NONE);
    assert_eq!(parsed.kind.name(), "none");
    assert_eq!(
        parsed.kind.core().expect("a core method").label(),
        "none (proxy-injected)"
    );
    assert!(parsed.borrow.is_empty());
    assert!(parsed.env.is_none());
    assert!(!parsed.store_only);
    assert!(parsed.validate().is_ok());
}

/// ADR-0212: a kind that is not the core's names a credential module; one this p1 has not
/// installed is refused when the route is used, naming the methods it has.
#[test]
fn an_unknown_kind_is_refused_when_used_and_the_error_lists_the_known_ones() {
    let kind = spec(r#"{"kind":"bearer-token"}"#).unwrap().kind;
    let error = Locations::none().method(&kind).err().unwrap();
    assert!(error.contains("bearer-token"), "{error}");
    for name in ["api-key", "none", "p1 modules install"] {
        assert!(error.contains(name), "{error}");
    }
    let error = spec(r#"{"kind":"Bearer Token"}"#).unwrap_err().to_string();
    assert!(error.contains("not a method name"), "{error}");
}

#[test]
fn an_unknown_key_in_the_table_is_rejected() {
    let error = spec(r#"{"kind":"api-key","env":"A_KEY","file":"/tmp/x"}"#)
        .unwrap_err()
        .to_string();
    assert!(error.contains("file"), "{error}");
}

#[test]
fn borrow_sources_are_store_and_key_pairs() {
    let parsed = spec(r#"{"kind":"api-key","env":"A_KEY","borrow":["opencode:go","pi:zai"]}"#)
        .expect("the table parses");
    assert_eq!(
        parsed.borrow,
        vec![
            BorrowSource {
                store: "opencode".to_string(),
                key: "go".to_string(),
            },
            BorrowSource {
                store: "pi".to_string(),
                key: "zai".to_string(),
            },
        ]
    );

    for (text, wanted) in [
        (
            r#"{"kind":"api-key","env":"A","borrow":["Keep Pass:x"]}"#,
            "not a credential module name",
        ),
        (
            r#"{"kind":"api-key","env":"A","borrow":["opencode"]}"#,
            "is not `<store>:<key>`",
        ),
        (
            r#"{"kind":"api-key","env":"A","borrow":["pi:"]}"#,
            "names no key",
        ),
    ] {
        let error = spec(text).unwrap_err().to_string();
        assert!(error.contains(wanted), "{text}: {error}");
    }
}

#[test]
fn an_api_key_route_must_name_its_variable() {
    let error = spec(r#"{"kind":"api-key","borrow":["pi:zai"]}"#)
        .expect("the table parses")
        .validate()
        .unwrap_err();
    assert!(error.contains("api-key"), "{error}");
    assert!(error.contains("env"), "{error}");
}

#[test]
fn a_variable_name_that_is_not_one_is_rejected() {
    for env in ["", "1_KEY", "A-KEY", "A KEY"] {
        let error = spec(&format!(r#"{{"kind":"api-key","env":"{env}"}}"#))
            .expect("the table parses")
            .validate()
            .unwrap_err();
        assert!(
            error.contains("not an environment variable name"),
            "{error}"
        );
    }
    assert!(
        spec(r#"{"kind":"api-key","env":"A_KEY"}"#)
            .unwrap()
            .validate()
            .is_ok()
    );
}
