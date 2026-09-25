//! Issue #134 END TO END through the host: a route file that declares
//! `[credential] kind = "none"` composes, for each of the three adapters, a provider
//! whose request carries NO authentication header — and reads neither p1's store nor
//! another tool's login.
//!
//! The route files are the SHIPPED ones with their `[credential]` table replaced, so
//! the test drives the same loader, the same catalog factory and the same adapters a
//! real route uses. The scratch home holds a p1 store entry 0644 (any read of it is
//! REFUSED with the chmod message) and DIRECTORY where each CLI's login file belongs
//! (any read of it reports that source unusable): a chain that touched either would
//! fail before a request existed, so "a request was sent" is itself the negative
//! proof. Every value is an obvious fake and no test opens a socket.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use p1_auth::{CredentialKind, Locations, describe};
use p1_contracts::{CancellationToken, Item, ModelOptions, Provider, ProviderRequest};
use p1_host::auth::credential_source_at;
use p1_host::catalog::route_provider;
use p1_host::routes::{RouteFile, load_route};
use p1_model_profile::ModelProfile;
use p1_provider_http::testing::{
    BodyEnd, RefusingWsConnector, ScriptedResponse, ScriptedTransport,
};

/// The store value that must never reach the wire.
const SENTINEL: &str = "FAKE-PROXY-STORE-SENTINEL-4d2a";

fn repo(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

/// `text` with its whole `[credential]` table replaced by `table`.
fn with_credential_table(text: &str, table: &str) -> String {
    let mut out = String::new();
    let mut replaced = false;
    let mut in_credential = false;
    for line in text.lines() {
        if line.trim_start().starts_with('[') {
            in_credential = line.trim() == "[credential]";
            if in_credential {
                out.push_str("[credential]\n");
                out.push_str(table);
                out.push('\n');
                replaced = true;
                continue;
            }
        }
        if in_credential {
            continue;
        }
        out.push_str(line);
        out.push('\n');
    }
    assert!(replaced, "the shipped route has a [credential] table");
    out
}

/// One shipped route file with its credential table replaced, loaded through the
/// host's own loader so it is validated exactly like a shipped one.
fn route_with(id: &str, table: &str) -> RouteFile {
    let shipped = std::fs::read_to_string(repo(&format!("routes/{id}.toml")))
        .unwrap_or_else(|error| panic!("routes/{id}.toml: {error}"));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(format!("{id}.toml"));
    std::fs::write(&path, with_credential_table(&shipped, table)).unwrap();
    load_route(&path).unwrap_or_else(|error| panic!("{id}: {error}"))
}

/// The shipped profile `id`, read from `profiles/<id>.toml`.
fn profile(id: &str) -> Arc<ModelProfile> {
    let text = std::fs::read_to_string(repo(&format!("profiles/{id}.toml")))
        .unwrap_or_else(|error| panic!("profiles/{id}.toml: {error}"));
    Arc::new(ModelProfile::from_toml(id, &text).unwrap_or_else(|error| panic!("{id}: {error}")))
}

/// A scratch home holding every source the chain could reach, in a shape that makes
/// any read FAIL: a 0644 p1 store entry, and a directory where each CLI's credential
/// file belongs.
struct Home {
    dir: tempfile::TempDir,
}

impl Home {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        for relative in [".local/share/opencode", ".claude", ".codex", ".pi"] {
            std::fs::create_dir_all(home.join(relative)).unwrap();
        }
        let store_dir = home.join(".config/p1");
        std::fs::create_dir_all(&store_dir).unwrap();
        // 0700, so the store FILE's own mode is the only refusal in play below.
        std::fs::set_permissions(&store_dir, fs::Permissions::from_mode(0o700)).unwrap();
        let store = serde_json::json!({
            "anthropic-subscription": { "type": "oauth", "access": SENTINEL },
            "openai-codex-subscription": { "type": "oauth", "access": SENTINEL },
            "glm-subscription": { "type": "api_key", "key": SENTINEL },
        })
        .to_string();
        std::fs::write(store_dir.join("auth.json"), &store).unwrap();
        std::fs::set_permissions(
            store_dir.join("auth.json"),
            fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        // Directories where the login FILES belong: a read reports the source unusable.
        for relative in [
            ".local/share/opencode/auth.json",
            ".claude/.credentials.json",
            ".codex/auth.json",
            ".pi/agent/auth.json",
        ] {
            std::fs::create_dir_all(home.join(relative)).unwrap();
        }
        Self { dir }
    }

    fn path(&self) -> PathBuf {
        self.dir.path().to_path_buf()
    }

    fn store(&self) -> String {
        std::fs::read_to_string(self.dir.path().join(".config/p1/auth.json")).unwrap()
    }
}

/// The provider the host's catalog factory builds for one route and one bound profile.
fn provider(
    route: &RouteFile,
    profile_id: &str,
    transport: ScriptedTransport,
    ws: Arc<dyn p1_provider_http::ws::WsConnector>,
    home: &Path,
) -> Arc<dyn Provider> {
    let binding = route
        .binding(profile_id)
        .expect("the route serves this profile")
        .clone();
    let locations = Locations::none().with_home(Some(home.to_path_buf()));
    let credentials = credential_source_at(route, Arc::new(transport.clone()), &locations);
    route_provider(
        route,
        &binding,
        profile(profile_id),
        Arc::new(transport),
        ws,
        credentials,
    )
    .expect("the route composes")
}

fn request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "sys".into(),
        history: vec![Item::User { text: "hi".into() }],
        tools: vec![],
        options: ModelOptions::default(),
    }
}

/// Every header name that carries (or names) a credential.
const CREDENTIAL_HEADERS: [&str; 5] = [
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "chatgpt-account-id",
];

/// Drive one request to the transport and return its header names. Polling stops as
/// soon as a request was recorded; the Codex route speaks WebSocket first and needs a
/// poll or two to fall back to SSE (the injected connector refuses every upgrade).
async fn posted_header_names(
    provider: &dyn Provider,
    transport: &ScriptedTransport,
) -> Vec<String> {
    let mut stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .expect("the request is buildable");
    for _ in 0..4 {
        if !transport.requests().is_empty() {
            break;
        }
        if stream.next().await.is_none() {
            break;
        }
    }
    let requests = transport.requests();
    assert_eq!(requests.len(), 1, "exactly one request must reach the wire");
    requests
        .into_iter()
        .next()
        .unwrap()
        .headers
        .iter()
        .map(|(name, _)| name.to_ascii_lowercase())
        .collect()
}

#[tokio::test(start_paused = true)]
async fn a_none_route_reaches_every_adapter_with_no_credential_header() {
    for (id, profile_id, adapter) in [
        (
            "anthropic-subscription",
            "claude-sonnet-5",
            "anthropic-messages",
        ),
        (
            "openai-codex-subscription",
            "gpt-5.6-sol",
            "openai-responses",
        ),
        ("glm-subscription", "glm-5.3", "openai-chat"),
    ] {
        let route = route_with(id, "kind = \"none\"\n");
        assert_eq!(route.adapter, adapter, "{id}");
        assert_eq!(route.credential.kind, CredentialKind::None, "{id}");

        let home = Home::new();
        let store_before = home.store();
        let transport = ScriptedTransport::new(
            (0..4)
                .map(|_| ScriptedResponse {
                    status: 200,
                    headers: Vec::new(),
                    chunks: Vec::new(),
                    end: BodyEnd::Eof,
                })
                .collect(),
        );
        let provider = provider(
            &route,
            profile_id,
            transport.clone(),
            Arc::new(RefusingWsConnector::default()),
            &home.path(),
        );
        let headers = posted_header_names(provider.as_ref(), &transport).await;

        for forbidden in CREDENTIAL_HEADERS {
            assert!(
                !headers.iter().any(|name| name == forbidden),
                "{id} sent {forbidden}: {headers:?}"
            );
        }
        // The store was never read (it is 0644, so a read would have failed before a
        // request existed) and never written.
        assert_eq!(home.store(), store_before, "{id}");
    }
}

#[tokio::test(start_paused = true)]
async fn the_same_route_still_sends_its_credential_when_the_table_names_one() {
    // The contrast that makes the negative claim mean something: the very same route
    // file, with an ordinary api-key table and a readable store, sends the bearer from
    // the store. Nothing about the adapter changed.
    let route = route_with(
        "glm-subscription",
        "kind       = \"api-key\"\nenv        = \"ZAI_API_KEY\"\nborrow     = []\nstore_only = true\n",
    );
    assert_eq!(route.credential.kind, CredentialKind::ApiKey);

    let home = Home::new();
    std::fs::set_permissions(
        home.path().join(".config/p1/auth.json"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let transport = ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: Vec::new(),
        end: BodyEnd::Eof,
    }]);
    let provider = provider(
        &route,
        "glm-5.3",
        transport.clone(),
        Arc::new(RefusingWsConnector::default()),
        &home.path(),
    );
    let mut stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .expect("the request is buildable");
    let mut events = Vec::new();
    for _ in 0..4 {
        match stream.next().await {
            Some(event) => events.push(event),
            None => break,
        }
        if !transport.requests().is_empty() {
            break;
        }
    }
    let requests = transport.requests();
    assert_eq!(requests.len(), 1, "{events:?}");
    let authorization = requests[0]
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.clone())
        .expect("an api-key route sends its credential");
    assert_eq!(authorization, format!("Bearer {SENTINEL}"));
}

// -------------------------------------------------------------- the route-file error

#[test]
fn an_unknown_credential_kind_is_still_a_route_file_error() {
    let route = with_credential_table(
        &std::fs::read_to_string(repo("routes/glm-subscription.toml")).unwrap(),
        "kind = \"bearer-token\"\n",
    );
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("glm-subscription.toml");
    std::fs::write(&path, route).unwrap();
    let error = load_route(&path).unwrap_err();
    assert!(error.contains("bearer-token"), "{error}");
    for known in ["api-key", "claude-code-oauth", "codex-oauth", "none"] {
        assert!(error.contains(known), "{error}");
    }
}

#[test]
fn a_none_route_may_not_also_name_a_source_in_a_route_file() {
    for (table, field) in [
        ("kind = \"none\"\nenv = \"ZAI_API_KEY\"\n", "env"),
        ("kind = \"none\"\nborrow = [\"pi:zai\"]\n", "borrow"),
        ("kind = \"none\"\nstore_only = true\n", "store_only"),
    ] {
        let text = with_credential_table(
            &std::fs::read_to_string(repo("routes/glm-subscription.toml")).unwrap(),
            table,
        );
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("glm-subscription.toml");
        std::fs::write(&path, text).unwrap();
        let error = load_route(&path).unwrap_err();
        assert!(error.contains(field), "{table}: {error}");
    }
}

/// The report the host prints for a `none` route: no source in the chain, and the
/// kind spelled as the operator sees it. Nothing is read, so the scratch home's
/// store — which a read would refuse — does not matter.
#[test]
fn a_none_route_reports_proxy_injected_and_no_source() {
    let route = route_with("glm-subscription", "kind = \"none\"\n");
    let home = Home::new();
    let locations = Locations::none().with_home(Some(home.path()));
    let report = describe(&route.id, &route.credential, &locations);
    assert!(report.tried.is_empty(), "{:?}", report.tried);
    assert_eq!(report.chosen, None);
    assert!(
        report.line().contains("none (proxy-injected)"),
        "{}",
        report.line()
    );
    assert_eq!(route.credential.kind.label(), "none (proxy-injected)");
}
