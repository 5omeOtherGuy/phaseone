//! The borrowed CLI logins: the two stores' entry shapes, a key another program
//! rotated, and the errors a bad entry produces. Fake values in scratch files only.

mod support;

use p1_auth::SubscriptionCredentials;
use p1_contracts::ProviderErrorKind;
use p1_provider_http::CredentialSource;
use support::Scratch;

#[tokio::test]
async fn rereads_rotated_keys_without_modifying_the_file() {
    for (kind, opencode) in [("api", true), ("api_key", false)] {
        let scratch = Scratch::new();
        let path = scratch.path("auth.json");
        scratch.write(
            "auth.json",
            &serde_json::json!({ "test": { "type": kind, "key": "FAKE-first" }, "untouched": true })
                .to_string(),
        );
        let source = SubscriptionCredentials::from_file(path.clone(), "test", opencode);

        let old = source.access().await.unwrap();
        assert!(!format!("{source:?} {old:?}").contains("FAKE-first"));
        assert!(
            source.refresh(&old).await.is_err(),
            "the same key is rejected"
        );

        let updated =
            serde_json::json!({ "test": { "type": kind, "key": "FAKE-second" }, "untouched": true })
                .to_string();
        scratch.write("auth.json", &updated);
        assert_eq!(source.refresh(&old).await.unwrap().bearer, "FAKE-second");
        assert_eq!(source.access().await.unwrap().bearer, "FAKE-second");
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            updated,
            "reading never writes"
        );
    }
}

#[tokio::test]
async fn rejects_bad_files_and_command_keys_without_disclosing_content() {
    let scratch = Scratch::new();
    let source = SubscriptionCredentials::from_file(scratch.path("auth.json"), "test", false);
    for content in [
        "PRIVATE-invalid-json".to_string(),
        serde_json::json!({ "test": { "type": "api_key", "key": "!PRIVATE-command" } }).to_string(),
        serde_json::json!({ "test": { "type": "api_key", "key": "PRIVATE\ninvalid" } }).to_string(),
        serde_json::json!({ "test": { "type": "oauth", "key": "PRIVATE-wrong-type" } }).to_string(),
    ] {
        scratch.write("auth.json", &content);
        let error = source.access().await.unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        assert!(
            !error.to_string().contains("PRIVATE"),
            "{}",
            error.to_string()
        );
    }
}

#[tokio::test]
async fn a_missing_file_or_entry_names_the_store_and_what_to_do() {
    let scratch = Scratch::new();
    let source = SubscriptionCredentials::from_file(scratch.path("auth.json"), "test", true);

    let error = source.access().await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert!(error.message.contains("opencode"), "{}", error.message);
    assert!(
        !scratch.path("auth.json").exists(),
        "reading writes nothing"
    );
}
