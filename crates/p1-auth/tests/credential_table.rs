//! The `[credential]` table itself (spec §2): the file format is unchanged, and
//! the one thing this step added is that an `api-key` route must NAME the variable
//! that holds its key.

use p1_auth::{BorrowSource, BorrowStore, CredentialKind, CredentialSpec};

fn spec(json: &str) -> Result<CredentialSpec, serde_json::Error> {
    serde_json::from_str(json)
}

#[test]
fn the_three_kinds_parse_by_their_route_file_spelling() {
    for (text, kind, name) in [
        (
            r#"{"kind":"api-key","env":"A_KEY"}"#,
            CredentialKind::ApiKey,
            "api-key",
        ),
        (
            r#"{"kind":"claude-code-oauth"}"#,
            CredentialKind::ClaudeCodeOauth,
            "claude-code-oauth",
        ),
        (
            r#"{"kind":"codex-oauth"}"#,
            CredentialKind::CodexOauth,
            "codex-oauth",
        ),
    ] {
        let parsed = spec(text).expect("the table parses");
        assert_eq!(parsed.kind, kind);
        assert_eq!(parsed.kind.name(), name);
        assert!(parsed.borrow.is_empty());
    }
}

#[test]
fn an_unknown_kind_is_rejected_and_the_error_lists_the_known_ones() {
    let error = spec(r#"{"kind":"bearer-token"}"#).unwrap_err().to_string();
    assert!(error.contains("bearer-token"), "{error}");
    for name in ["api-key", "claude-code-oauth", "codex-oauth"] {
        assert!(error.contains(name), "{error}");
    }
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
                store: BorrowStore::Opencode,
                key: "go".to_string(),
            },
            BorrowSource {
                store: BorrowStore::Pi,
                key: "zai".to_string(),
            },
        ]
    );

    for (text, wanted) in [
        (
            r#"{"kind":"api-key","env":"A","borrow":["keepass:x"]}"#,
            "unknown borrow store",
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
