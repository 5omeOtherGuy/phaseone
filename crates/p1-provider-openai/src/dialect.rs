//! The Responses dialect port (ADR-0211): the wire facts one family of accounts requires, in
//! one trait the adapter asks instead of matching a closed list of accounts.
//!
//! The adapter's core dialect is [`ApiKey`]; the subscription connectors add the Codex
//! subscription (`codex-subscription`) as an implementation in a build with the
//! `subscriptions` feature. [`dialects`] is the one place a build's dialects are listed.

/// One Responses dialect.
pub trait ResponsesDialect: Send + Sync + 'static {
    /// The spelling a route file uses (`dialect = "…"`).
    fn name(&self) -> &'static str;

    /// The responses path relative to the endpoint and the version segment before it, e.g.
    /// (`/v1/responses`, `/v1`).
    fn paths(&self) -> (&'static str, &'static str);

    /// The header that names the account, when the account needs one: a credential without
    /// that id is an authentication failure.
    fn account_id_header(&self) -> Option<&'static str> {
        None
    }

    /// Whether the wire takes an output-cap field; an explicit cap on one that does not is an
    /// error rather than a field the server would ignore.
    fn carries_output_cap(&self) -> bool {
        true
    }

    /// Whether requests carry the Codex client's identity: the `originator` header, the
    /// `OpenAI-Beta` value and the session headers.
    fn is_codex(&self) -> bool {
        false
    }
}

/// The public Responses API with an OpenAI API key (ADR-0197).
pub(crate) struct ApiKey;

impl ResponsesDialect for ApiKey {
    fn name(&self) -> &'static str {
        "api-key"
    }

    fn paths(&self) -> (&'static str, &'static str) {
        ("/v1/responses", "/v1")
    }
}

/// Every Responses dialect this build has: the subscription connectors' when the build has
/// them, then the core's. The one place they are listed; nothing registers itself.
pub fn dialects() -> Vec<&'static dyn ResponsesDialect> {
    vec![
        #[cfg(feature = "subscriptions")]
        &crate::subscriptions::CodexSubscription,
        &ApiKey,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ResponsesAccount, ResponsesAdapterSettings};

    #[test]
    fn a_build_lists_its_dialects_and_a_route_names_one() {
        let names: Vec<&str> = dialects().iter().map(|dialect| dialect.name()).collect();
        #[cfg(feature = "subscriptions")]
        assert_eq!(names, ["codex-subscription", "api-key"]);
        #[cfg(not(feature = "subscriptions"))]
        assert_eq!(names, ["api-key"]);
        let settings: ResponsesAdapterSettings =
            serde_json::from_str(r#"{"dialect":"api-key"}"#).expect("the core dialect");
        assert_eq!(settings.account, ResponsesAccount::API_KEY);
        let error = serde_json::from_str::<ResponsesAdapterSettings>(r#"{"dialect":"pigeon"}"#)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("pigeon") && error.contains("api-key"),
            "{error}"
        );
    }
}
