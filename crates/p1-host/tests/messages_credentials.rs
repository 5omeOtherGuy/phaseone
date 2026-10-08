use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_host::routes::{check_credential_origin, load_route};
use p1_provider_http::testing::ScriptedTransport;

fn repo(path: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(path)
}

#[tokio::test]
async fn messages_accounts_reuse_existing_store_entries_not_new_copies() {
    let home = tempfile::tempdir().unwrap();
    let locations = p1_auth::Locations::none().with_home(Some(home.path().to_owned()));
    for suffix in ["", "-1", "-2", "-3"] {
        let source_id = format!("opencode-go{suffix}-subscription");
        let route =
            load_route(&repo(&format!("routes/opencode-go-messages{suffix}.toml"))).unwrap();
        let original = load_route(&repo(&format!("routes/{source_id}.toml"))).unwrap();
        assert_eq!(route.credential, original.credential);
        assert_eq!(route.credential_route_id(), source_id);
        assert_ne!(route.origin_route, original.origin_route);
        p1_auth::store::put_api_key_at_origin(
            &source_id,
            "FAKE-SHARED-KEY",
            Some("https://opencode.ai"),
            &locations,
        )
        .await
        .unwrap();
        check_credential_origin(&route, &locations).unwrap();
        let credentials = p1_host::auth::credential_source_at(
            &route,
            Arc::new(ScriptedTransport::new(vec![])),
            &locations,
        );
        assert!(credentials.access().await.unwrap().bearer == "FAKE-SHARED-KEY");
        assert!(
            p1_auth::describe(&route.id, &route.credential, &locations)
                .chosen
                .is_none()
        );
    }
}

#[test]
fn a_store_alias_cannot_redirect_a_shipped_key_to_another_origin_or_kind() {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::read_to_string(repo("routes/opencode-go-messages.toml"))
        .unwrap()
        .replace("id = \"opencode-go-messages\"", "id = \"custom\"");
    for text in [
        base.replace(
            "https://opencode.ai/zen/go",
            "https://attacker.example/zen/go",
        ),
        base.replace("opencode-go-subscription", "anthropic-subscription"),
        base.replace("store_only = true", "store_only = false"),
        base.replace("opencode-go-subscription", "missing-route"),
    ] {
        let path = dir.path().join("custom.toml");
        std::fs::write(&path, text).unwrap();
        assert!(load_route(&path).unwrap_err().contains("credential_route"));
    }
}

#[test]
fn messages_credential_placement_is_explicit_and_cannot_change_claude() {
    let dir = tempfile::tempdir().unwrap();
    let base = std::fs::read_to_string(repo("routes/opencode-go-messages.toml")).unwrap();
    for text in [
        base.replace("credential_header = \"x-api-key\"", ""),
        base.replace(
            "credential_header = \"x-api-key\"",
            "credential_header = \"cookie\"",
        ),
        base.replace(
            "account = \"opencode-go\"",
            "account = \"claude-code-subscription\"",
        ),
    ] {
        let path = dir.path().join("route.toml");
        std::fs::write(&path, text).unwrap();
        assert!(load_route(&path).unwrap_err().contains("credential_header"));
    }
}

#[test]
fn the_new_environment_reuses_the_prompt_and_matches_deepseek_configuration() {
    assert!(
        std::fs::symlink_metadata(repo("environments/deepseek-messages/prompt.md"))
            .unwrap()
            .file_type()
            .is_file()
    );
    let read = |name: &str| {
        std::fs::read_to_string(repo(&format!("environments/{name}/environment.toml"))).unwrap()
    };
    let mut original: toml::Value = toml::from_str(&read("deepseek")).unwrap();
    let messages: toml::Value = toml::from_str(&read("deepseek-messages")).unwrap();
    original["route"] = toml::Value::String("opencode-go-messages".into());
    assert_eq!(messages, original);
    assert_eq!(
        std::fs::read(repo("environments/deepseek-messages/prompt.md")).unwrap(),
        std::fs::read(repo("environments/deepseek/prompt.md")).unwrap()
    );
}
