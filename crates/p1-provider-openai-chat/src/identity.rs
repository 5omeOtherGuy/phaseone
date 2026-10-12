//! The chat client-identity port (ADR-0211): a non-secret identity a route presents to a
//! vendor gateway that gates its free tier on the caller looking like the vendor's own client.
//!
//! The core has none; the subscription connectors add the OpenCode CLI's (`opencode`) in a
//! build with the `subscriptions` feature. [`client_identities`] is the one place a build's
//! identities are listed.

/// One client identity. It changes no message encoding and declares no tool: it only adds
/// its static headers and the ids it derives for a request.
pub trait ChatClientIdentity: Send + Sync + 'static {
    /// The spelling a route file uses (`client_identity = "…"`).
    fn name(&self) -> &'static str;

    /// The static header names the identity owns: a route may not send a second value for
    /// one of them.
    fn owned_headers(&self) -> &'static [&'static str];

    /// The headers the identity adds to a request: static, non-secret values, plus ids
    /// derived from the request's cache key (fresh ones without a key).
    fn headers(&self, cache_key: Option<&str>) -> Vec<(String, String)>;
}

/// Every client identity this build has. The one place they are listed; nothing registers
/// itself.
pub fn client_identities() -> Vec<&'static dyn ChatClientIdentity> {
    #[cfg(feature = "subscriptions")]
    return vec![&crate::subscriptions::Opencode];
    #[cfg(not(feature = "subscriptions"))]
    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ChatAdapterSettings;

    #[test]
    fn a_build_lists_its_identities_and_a_route_names_one() {
        let names: Vec<&str> = client_identities()
            .iter()
            .map(|identity| identity.name())
            .collect();
        #[cfg(feature = "subscriptions")]
        {
            assert_eq!(names, ["opencode"]);
            let settings: ChatAdapterSettings =
                serde_json::from_str(r#"{"dialect":"effort-level","client_identity":"opencode"}"#)
                    .expect("the connectors' identity");
            assert_eq!(settings.client_identity, Some(crate::OPENCODE));
        }
        #[cfg(not(feature = "subscriptions"))]
        assert!(names.is_empty());
        let error = serde_json::from_str::<ChatAdapterSettings>(
            r#"{"dialect":"effort-level","client_identity":"pigeon"}"#,
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("pigeon"), "{error}");
    }
}
