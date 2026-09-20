//! Borrowed subscription key sources (ADR-0040). The p1 store is a later migration.
use p1_contracts::{BoxFuture, ProviderError, ProviderErrorKind};
use p1_provider_http::{Credential, CredentialSource};
use std::path::PathBuf;

/// Read-only API key reuse. No shell commands, refresh endpoint or credential writes.
/// Environment overrides precede OpenCode/Pi auth files. Files are re-read on access.
pub struct SubscriptionCredentials {
    env_var: Option<String>,
    files: Vec<(PathBuf, String, String)>,
}
impl std::fmt::Debug for SubscriptionCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SubscriptionCredentials { <redacted> }")
    }
}
fn auth(message: &str) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Authentication, message)
}
impl SubscriptionCredentials {
    pub fn opencode_go() -> Self {
        Self::borrowed("OPENCODE_API_KEY", "opencode-go", true)
    }
    pub fn glm() -> Self {
        Self::borrowed("ZAI_API_KEY", "zai", false)
    }
    fn borrowed(env_var: &str, pi_key: &str, opencode: bool) -> Self {
        let home = std::env::var_os("HOME").map(PathBuf::from);
        let mut files = Vec::new();
        if opencode {
            let data = std::env::var_os("XDG_DATA_HOME")
                .map(PathBuf::from)
                .or_else(|| home.as_ref().map(|h| h.join(".local/share")));
            if let Some(data) = data {
                files.push((data.join("opencode/auth.json"), pi_key.into(), "api".into()));
            }
        }
        let pi_dir = std::env::var_os("PI_CODING_AGENT_DIR")
            .map(PathBuf::from)
            .or_else(|| home.map(|h| h.join(".pi/agent")));
        if let Some(dir) = pi_dir {
            files.push((dir.join("auth.json"), pi_key.into(), "api_key".into()));
        }
        Self {
            env_var: Some(env_var.into()),
            files,
        }
    }
    /// Explicit location for embedding and isolated tests; uses no process environment.
    pub fn from_file(path: PathBuf, provider: &str, opencode_format: bool) -> Self {
        Self {
            env_var: None,
            files: vec![(
                path,
                provider.into(),
                if opencode_format { "api" } else { "api_key" }.into(),
            )],
        }
    }
    fn read(&self) -> Result<Credential, ProviderError> {
        if let Some(name) = &self.env_var {
            match std::env::var(name) {
                Ok(value) => return key(value),
                Err(std::env::VarError::NotUnicode(_)) => {
                    return Err(auth("subscription key is not Unicode"));
                }
                Err(std::env::VarError::NotPresent) => {}
            }
        }
        for (path, provider, kind) in &self.files {
            let bytes = match std::fs::read(path) {
                Ok(bytes) => bytes,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Err(auth("cannot read subscription credential file")),
            };
            let data: serde_json::Value = serde_json::from_slice(&bytes)
                .map_err(|_| auth("invalid subscription credential file"))?;
            let Some(entry) = data.get(provider) else {
                continue;
            };
            if entry.get("type").and_then(|v| v.as_str()) != Some(kind.as_str()) {
                return Err(auth("subscription requires an API key credential"));
            }
            let value = entry
                .get("key")
                .and_then(|v| v.as_str())
                .ok_or_else(|| auth("subscription credential has no key"))?;
            // Pi permits command-backed keys; executing configuration is outside this credential source.
            if value.starts_with('!') {
                return Err(auth(
                    "command-backed keys are unsupported; set the documented key environment variable",
                ));
            }
            return key(value.into());
        }
        Err(auth(
            "no subscription credential found; set the documented key environment variable or log in with the subscription CLI",
        ))
    }
}
fn key(value: String) -> Result<Credential, ProviderError> {
    if value.is_empty() || !value.bytes().all(|b| (33..=126).contains(&b)) {
        return Err(auth("invalid subscription key format"));
    }
    Ok(Credential {
        bearer: value,
        account_id: None,
    })
}
impl CredentialSource for SubscriptionCredentials {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move { self.read() })
    }
    fn refresh<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            let current = self.read()?;
            if current.bearer == rejected.bearer {
                return Err(auth(
                    "subscription key rejected; update the CLI login or key environment variable",
                ));
            }
            Ok(current)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[tokio::test]
    async fn rereads_rotated_keys_without_modifying_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        for (kind, opencode) in [("api", true), ("api_key", false)] {
            std::fs::write(
                &path,
                json!({"test":{"type":kind,"key":"FAKE-first"},"untouched":true}).to_string(),
            )
            .unwrap();
            let source = SubscriptionCredentials::from_file(path.clone(), "test", opencode);
            let old = source.access().await.unwrap();
            assert!(!format!("{source:?} {old:?}").contains("FAKE-first"));
            assert!(source.refresh(&old).await.is_err());
            let updated =
                json!({"test":{"type":kind,"key":"FAKE-second"},"untouched":true}).to_string();
            std::fs::write(&path, &updated).unwrap();
            assert_eq!(source.refresh(&old).await.unwrap().bearer, "FAKE-second");
            assert_eq!(source.access().await.unwrap().bearer, "FAKE-second");
            assert_eq!(std::fs::read_to_string(&path).unwrap(), updated);
        }
    }
    #[tokio::test]
    async fn rejects_bad_files_and_command_keys_without_disclosing_content() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let source = SubscriptionCredentials::from_file(path.clone(), "test", false);
        for content in [
            "PRIVATE-invalid-json".into(),
            json!({"test":{"type":"api_key","key":"!PRIVATE-command"}}).to_string(),
            json!({"test":{"type":"api_key","key":"PRIVATE\ninvalid"}}).to_string(),
        ] {
            std::fs::write(&path, content).unwrap();
            let error = source.access().await.unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::Authentication);
            assert!(!error.to_string().contains("PRIVATE"));
        }
    }
}

#[cfg(test)]
mod precedence_tests {
    use super::*;
    #[test]
    fn environment_override_child() {
        let Some(path) = std::env::var_os("P1_TEST_AUTH_FILE") else {
            return;
        };
        let source = SubscriptionCredentials {
            env_var: Some("P1_TEST_SUBSCRIPTION_KEY".into()),
            files: vec![(path.into(), "test".into(), "api_key".into())],
        };
        assert_eq!(source.read().unwrap().bearer, "FAKE-environment");
    }
    #[test]
    fn explicit_environment_key_wins_over_the_borrowed_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        std::fs::write(&path, r#"{"test":{"type":"api_key","key":"FAKE-file"}}"#).unwrap();
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "auth::precedence_tests::environment_override_child",
            ])
            .env("P1_TEST_AUTH_FILE", path)
            .env("P1_TEST_SUBSCRIPTION_KEY", "FAKE-environment")
            .status()
            .unwrap();
        assert!(status.success());
    }
}
