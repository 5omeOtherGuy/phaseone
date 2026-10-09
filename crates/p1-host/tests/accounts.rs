//! Accounts separate from routes (ADR-0139, issue #634): one route used with two
//! accounts, one account used by two routes, origin refusal before any request, and a
//! user account on shipped routes and profiles. Scratch directories and fake keys only;
//! every request goes to a scripted transport.
mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Harness, run_args, shipped_environments};
use p1_host::routes::{load_all_routes, load_route_by_id, load_route_pairs};
use p1_provider_http::testing::{ScriptedResponse, ScriptedTransport};

const ORIGIN: &str = "http://127.0.0.1:9";
const SSE: &str = "data: {\"id\":\"one\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"one\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

fn write(path: PathBuf, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A wire without a credential of its own: the account comes from elsewhere.
fn route(root: &Path, id: &str, path: &str, extra: &str) {
    write(
        root.join(format!("routes/{id}.toml")),
        &format!(
            "id = \"{id}\"\norigin_route = \"openai-chat/{id}\"\nadapter = \"openai-chat\"\n\
             endpoint = \"{ORIGIN}{path}\"\n{extra}\n[adapter_settings]\n\
             dialect = \"retained-thinking\"\n[models.model]\nwire_model = \"model\"\n"
        ),
    );
}

fn account(root: &Path, id: &str, origin: &str, env: &str) {
    write(
        root.join(format!("accounts/{id}.toml")),
        &format!(
            "id = \"{id}\"\norigins = [\"{origin}\"]\n[credential]\nmethod = \"api-key\"\n\
             env = \"{env}\"\nstore_only = true\n"
        ),
    );
}

fn environment(root: &Path, name: &str, route: &str, account: Option<&str>) {
    let account = account
        .map(|id| format!("account = \"{id}\"\n"))
        .unwrap_or_default();
    write(
        root.join(format!("environments/{name}/environment.toml")),
        &format!("route = \"{route}\"\n{account}profile = \"model\"\n"),
    );
    write(root.join(format!("environments/{name}/prompt.md")), "test");
}

/// Two wires on one loopback origin and two accounts that both declare it.
fn scratch() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(
        root.join("profiles/model.toml"),
        "id = \"model\"\nrevision = 1\nmodel_id = \"model\"\nfamily = \"test\"\n\
         thinking = \"enabled\"\nefforts = [\"high\"]\ndefault_effort = \"high\"\n",
    );
    route(root, "wire-a", "/a/chat/completions", "");
    route(root, "wire-b", "/b/chat/completions", "");
    account(root, "one", ORIGIN, "ONE_KEY");
    account(root, "two", ORIGIN, "TWO_KEY");
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("environments")).unwrap();
    dir
}

fn harness(root: &Path) -> Harness {
    let mut harness = Harness::new(vec![root.join("environments")], &["go", "/exit"]);
    harness.deps.home = Some(root.join("home"));
    harness.deps.shell_env = Some(vec![
        ("ONE_KEY".into(), "FAKE-one".into()),
        ("TWO_KEY".into(), "FAKE-two".into()),
    ]);
    harness
}

/// Run one turn of `env` and return the one request it sent.
async fn request_of(root: &Path, env: &str) -> (String, String) {
    let mut harness = harness(root);
    let transport = Arc::new(ScriptedTransport::new(vec![ScriptedResponse::ok_sse(SSE)]));
    harness.deps.transport = transport.clone();
    let code = run_args(
        &mut harness,
        &["--env", env, "--workspace", root.to_str().unwrap()],
    )
    .await;
    assert_eq!(code, 0, "{env}: {}", harness.stderr.text());
    let requests = transport.requests();
    assert_eq!(requests.len(), 1, "{env}");
    let authorization = requests[0]
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.clone())
        .expect("an authorization header");
    (requests[0].url.clone(), authorization)
}

#[tokio::test]
async fn one_route_with_two_accounts_and_one_account_on_two_routes() {
    let dir = scratch();
    let root = dir.path();
    environment(root, "a-one", "wire-a", Some("one"));
    environment(root, "a-two", "wire-a", Some("two"));
    environment(root, "b-one", "wire-b", Some("one"));
    let cases = [
        ("a-one", "/a/", "FAKE-one"),
        ("a-two", "/a/", "FAKE-two"),
        ("b-one", "/b/", "FAKE-one"),
    ];
    for (env, path, key) in cases {
        let (url, authorization) = request_of(root, env).await;
        assert!(url.contains(path), "{env}: {url}");
        assert_eq!(authorization, format!("Bearer {key}"), "{env}");
    }
    // Each pair has its own replay origin and store identity.
    let dirs = [root.join("environments")];
    let a_one = load_route_by_id(&dirs, "wire-a@one").unwrap();
    let a_two = load_route_by_id(&dirs, "wire-a@two").unwrap();
    assert_eq!(a_one.origin_route, "openai-chat/wire-a@one");
    assert_eq!(a_two.origin_route, "openai-chat/wire-a@two");
    assert_eq!(
        (a_one.store_id.as_str(), a_two.store_id.as_str()),
        ("one", "two")
    );
    assert_eq!(a_one.route, "wire-a");
    assert_eq!(a_one.account, "one");
    assert!(a_one.account_source.ends_with("accounts/one.toml"));
    let keys: Vec<String> = load_route_pairs(&dirs)
        .unwrap()
        .into_iter()
        .map(|route| route.id)
        .collect();
    assert_eq!(
        keys,
        ["wire-a@one", "wire-a@two", "wire-b@one", "wire-b@two"]
    );
}

#[tokio::test]
async fn an_account_that_does_not_declare_the_origin_is_refused_before_any_request() {
    let dir = scratch();
    let root = dir.path();
    account(root, "far", "https://far.example", "ONE_KEY");
    environment(root, "far", "wire-a", Some("far"));
    let mut harness = harness(root);
    let transport = Arc::new(ScriptedTransport::new(Vec::new()));
    harness.deps.transport = transport.clone();
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "far",
            "--workspace",
            root.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_ne!(code, 0);
    let error = harness.stderr.text();
    assert!(
        error.contains("account `far` does not list the endpoint origin http://127.0.0.1:9"),
        "{error}"
    );
    assert!(transport.requests().is_empty());
}

#[test]
fn selection_follows_the_route_default_and_the_only_covering_account() {
    let dir = scratch();
    let root = dir.path();
    let dirs = [root.join("environments")];
    // Two accounts declare the origin and the route names none: no primary binding.
    assert!(load_all_routes(&dirs).unwrap().is_empty());
    let error = load_route_by_id(&dirs, "wire-a").unwrap_err();
    assert!(error.contains("candidates: one, two"), "{error}");
    // A default account on the route picks it.
    route(root, "wire-a", "/a/chat/completions", "account = \"two\"");
    assert_eq!(load_route_by_id(&dirs, "wire-a").unwrap().account, "two");
    // With one covering account left, it is chosen without configuration.
    std::fs::remove_file(root.join("accounts/two.toml")).unwrap();
    let bound = load_route_by_id(&dirs, "wire-b").unwrap();
    assert_eq!(
        (bound.id.as_str(), bound.account.as_str()),
        ("wire-b", "one")
    );
    // An unknown account names the ones that would do.
    let error = load_route_by_id(&dirs, "wire-b@ghost").unwrap_err();
    assert!(
        error.contains("account `ghost`") && error.contains("one"),
        "{error}"
    );
}

#[test]
fn inline_credentials_are_implicit_accounts_and_conflicts_are_load_errors() {
    let dir = scratch();
    let root = dir.path();
    let dirs = [root.join("environments")];
    route(
        root,
        "legacy",
        "/legacy/chat/completions",
        "[credential]\nkind = \"api-key\"\nenv = \"ONE_KEY\"\nstore_only = true",
    );
    let legacy = load_route_by_id(&dirs, "legacy").unwrap();
    assert_eq!(legacy.account, "legacy");
    assert_eq!(legacy.store_id, "legacy");
    assert_eq!(legacy.origin_route, "openai-chat/legacy");
    // The implicit account is an account like any other: it serves another wire.
    let other = load_route_by_id(&dirs, "wire-a@legacy").unwrap();
    assert_eq!(other.store_id, "legacy");
    assert_eq!(other.origin_route, "openai-chat/wire-a@legacy");
    // An account file and an inline credential with one id in one directory.
    account(root, "legacy", ORIGIN, "TWO_KEY");
    let error = load_all_routes(&dirs).unwrap_err();
    assert!(
        error.contains("account `legacy` is defined twice"),
        "{error}"
    );
    std::fs::remove_file(root.join("accounts/legacy.toml")).unwrap();
    // Inline credential and default account together.
    route(
        root,
        "both",
        "/both",
        "account = \"one\"\n[credential]\nkind = \"none\"",
    );
    let error = load_all_routes(&dirs).unwrap_err();
    assert!(
        error.contains("both an inline `[credential]` and a default `account`"),
        "{error}"
    );
}

#[test]
fn a_user_account_works_with_shipped_routes_and_profiles_without_copies() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user");
    write(
        user.join("accounts/mine.toml"),
        "id = \"mine\"\norigins = [\"https://opencode.ai\"]\n[credential]\nmethod = \"api-key\"\n\
         env = \"MINE_KEY\"\nstore_only = true\n",
    );
    write(
        user.join("environments/mine/environment.toml"),
        "route = \"opencode-go-subscription\"\naccount = \"mine\"\nprofile = \"deepseek-v4.1-flash\"\n",
    );
    write(user.join("environments/mine/prompt.md"), "test");
    let dirs = [user.join("environments"), shipped_environments()];
    // The shipped profile resolves for a user environment (issue #87).
    let mut environment = p1_assembly::load_environment("mine", &dirs).unwrap();
    assert_eq!(environment.provider, "opencode-go-subscription@mine");
    p1_host::catalog::resolve_environment(&mut environment, &dirs).unwrap();
    let bound = load_route_by_id(&dirs, &environment.provider).unwrap();
    assert_eq!(bound.route, "opencode-go-subscription");
    assert_eq!(bound.credential.env.as_deref(), Some("MINE_KEY"));
    assert_eq!(bound.store_id, "mine");
    assert_eq!(
        bound.origin_route,
        "openai-chat/opencode-go-subscription@mine"
    );
    // A user account is not shipped: its origin needs approval before its key is used.
    let locations = p1_auth::Locations::none().with_home(Some(dir.path().join("home")));
    let error = p1_host::routes::check_credential_origin(&bound, &locations).unwrap_err();
    assert!(
        error.contains("untrusted endpoint https://opencode.ai"),
        "{error}"
    );
}

#[test]
fn a_user_account_cannot_take_a_shipped_store_entry_to_another_origin() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user");
    write(
        user.join("accounts/opencode-go-2-subscription.toml"),
        "id = \"opencode-go-2-subscription\"\norigins = [\"https://far.example\"]\n\
         [credential]\nmethod = \"api-key\"\nenv = \"X_KEY\"\nstore_only = true\n",
    );
    write(
        user.join("routes/far.toml"),
        "id = \"far\"\norigin_route = \"openai-chat/far\"\nadapter = \"openai-chat\"\n\
         endpoint = \"https://far.example/v1\"\n[adapter_settings]\n\
         dialect = \"retained-thinking\"\n[models.m]\nwire_model = \"m\"\n",
    );
    std::fs::create_dir_all(user.join("environments")).unwrap();
    let dirs = [user.join("environments"), shipped_environments()];
    // Beside the shipped account that holds the entry, it is refused at load (ADR-0139 §1).
    let error = load_route_by_id(&dirs, "far@opencode-go-2-subscription").unwrap_err();
    assert!(
        error.contains("both use the store entry `opencode-go-2-subscription`"),
        "{error}"
    );
    // In place of that account, the compiled origins still bind it (§2).
    std::fs::remove_file(user.join("accounts/opencode-go-2-subscription.toml")).unwrap();
    write(
        user.join("accounts/opencode-go-2.toml"),
        "id = \"opencode-go-2\"\norigins = [\"https://far.example\"]\n\
         store_id = \"opencode-go-2-subscription\"\n\
         [credential]\nmethod = \"api-key\"\nenv = \"X_KEY\"\nstore_only = true\n",
    );
    let bound = load_route_by_id(&dirs, "far@opencode-go-2").unwrap();
    let error = p1_host::routes::check_shipped_origin(&bound, &p1_host::routes::shipped_origins())
        .unwrap_err();
    assert!(error.contains("overrides a route p1 ships"), "{error}");
}

#[tokio::test]
async fn env_show_and_models_name_the_files_and_mark_a_user_copy_of_a_shipped_id() {
    let dir = tempfile::tempdir().unwrap();
    let user = dir.path().join("user");
    let shipped_profile = shipped_environments().join("../profiles/deepseek-v4.1-flash.toml");
    write(
        user.join("profiles/deepseek-v4.1-flash.toml"),
        &std::fs::read_to_string(&shipped_profile).unwrap(),
    );
    write(
        user.join("accounts/mine.toml"),
        "id = \"mine\"\norigins = [\"https://opencode.ai\"]\n[credential]\nmethod = \"api-key\"\n\
         env = \"MINE_KEY\"\nstore_only = true\n",
    );
    write(
        user.join("environments/mine/environment.toml"),
        "route = \"opencode-go-subscription\"\naccount = \"mine\"\nprofile = \"deepseek-v4.1-flash\"\n",
    );
    write(user.join("environments/mine/prompt.md"), "test");
    let mut harness = Harness::new(vec![user.join("environments"), shipped_environments()], &[]);
    harness.deps.home = Some(dir.path().join("home"));
    common::isolated_environment(&mut harness);
    assert_eq!(
        run_args(&mut harness, &["env", "show", "mine"]).await,
        0,
        "{}",
        harness.stderr.text()
    );
    let shown = harness.stdout.text();
    let line = |kind: &str| {
        shown
            .lines()
            .find(|line| line.starts_with(&format!("source  {kind} ")))
            .unwrap_or_else(|| panic!("no {kind} source line: {shown}"))
            .to_string()
    };
    assert!(line("environment").contains("user/environments/mine/environment.toml"));
    assert!(line("route").contains("routes/opencode-go-subscription.toml"));
    assert!(!line("route").contains("shadows"), "{shown}");
    let profile = line("profile");
    assert!(
        profile.contains("user/environments/../profiles/deepseek-v4.1-flash.toml")
            && profile.contains("(shadows ")
            && profile.contains("profiles/deepseek-v4.1-flash.toml)"),
        "{profile}"
    );
    assert!(
        line("account").contains("user/environments/../accounts/mine.toml"),
        "{shown}"
    );

    let mut harness = Harness::new(vec![user.join("environments"), shipped_environments()], &[]);
    harness.deps.home = Some(dir.path().join("home"));
    common::isolated_environment(&mut harness);
    assert_eq!(run_args(&mut harness, &["models", "mine/"]).await, 0);
    let listed = harness.stdout.text();
    let row = listed
        .lines()
        .find(|line| line.starts_with("mine/deepseek-v4.1-flash "))
        .unwrap_or_else(|| panic!("{listed}"));
    assert!(row.contains("shadows:profile"), "{row}");
}

#[test]
fn an_account_file_in_a_higher_directory_replaces_a_routes_inline_credential() {
    let dir = scratch();
    let shipped = dir.path();
    route(
        shipped,
        "legacy",
        "/legacy/chat/completions",
        "[credential]\nkind = \"api-key\"\nenv = \"ONE_KEY\"\nstore_only = true",
    );
    let user = tempfile::tempdir().unwrap();
    account(user.path(), "legacy", ORIGIN, "TWO_KEY");
    std::fs::remove_file(shipped.join("accounts/two.toml")).unwrap();
    std::fs::create_dir_all(user.path().join("environments")).unwrap();
    let dirs = [
        user.path().join("environments"),
        shipped.join("environments"),
    ];
    // One account id resolves one way, whichever key names it.
    for key in ["legacy", "legacy@legacy", "wire-a@legacy"] {
        let bound = load_route_by_id(&dirs, key).unwrap();
        assert_eq!(bound.credential.env.as_deref(), Some("TWO_KEY"), "{key}");
        assert_eq!(bound.credential_route_id(), "legacy", "{key}");
    }
}

#[test]
fn a_route_id_with_an_account_separator_is_a_load_error() {
    let dir = scratch();
    route(dir.path(), "a@b", "/x", "[credential]\nkind = \"none\"");
    let error = load_all_routes(&[dir.path().join("environments")]).unwrap_err();
    assert!(error.contains("contains `@`"), "{error}");
}

struct NoEcho;

impl p1_host::login::EchoControl for NoEcho {
    fn disable(&self) -> Result<p1_host::login::Guard, String> {
        Ok(p1_host::login::Guard::from_restore(|| {}))
    }
}

fn origins_file(root: &Path) -> serde_json::Value {
    serde_json::from_str(
        &std::fs::read_to_string(root.join("home/.config/p1/auth.json.origins")).unwrap(),
    )
    .unwrap()
}

#[tokio::test]
async fn login_trust_and_logout_act_on_accounts_and_approve_their_declared_origins() {
    let dir = scratch();
    let root = dir.path();
    write(
        root.join("accounts/multi.toml"),
        "id = \"multi\"\nlabel = \"two hosts\"\norigins = [\"http://127.0.0.1:9\", \
         \"https://eu.example.test\"]\n[credential]\nmethod = \"api-key\"\nenv = \"MULTI_KEY\"\n\
         store_only = true\n",
    );
    let locations = p1_auth::Locations::none().with_home(Some(root.join("home")));
    // A pasted key for an account file: its own store entry, one origin as a string.
    let mut harness = Harness::new(vec![root.join("environments")], &["FAKE-STORED"]);
    harness.deps.home = Some(root.join("home"));
    harness.deps.shell_env = Some(Vec::new());
    assert_eq!(
        p1_host::login::login_with(&harness.deps, "one", false, &NoEcho).await,
        0,
        "{}",
        harness.stderr.text()
    );
    assert!(harness.stdout.text().contains("stored for one"));
    assert_eq!(origins_file(root)["one"], serde_json::json!(ORIGIN));
    // Approval of an account with two origins records both, as an array.
    assert_eq!(
        p1_host::login::trust_endpoint(&harness.deps, "multi").await,
        0,
        "{}",
        harness.stderr.text()
    );
    assert_eq!(
        origins_file(root)["multi"],
        serde_json::json!([ORIGIN, "https://eu.example.test"])
    );
    assert_eq!(
        p1_auth::store::endpoint_origins("multi", &locations).unwrap(),
        [ORIGIN, "https://eu.example.test"]
    );
    // The listing has one row per account; the route without an account of its own
    // adds none.
    let mut harness = Harness::new(vec![root.join("environments")], &[]);
    harness.deps.home = Some(root.join("home"));
    harness.deps.shell_env = Some(Vec::new());
    assert_eq!(p1_host::login::list(&harness.deps), 0);
    let listed = harness.stdout.text();
    let ids: Vec<&str> = listed
        .lines()
        .map(|line| line.split_whitespace().next().unwrap())
        .collect();
    assert_eq!(ids, ["multi", "one", "two"], "{listed}");
    // Logout removes the account's entry and its approval.
    assert_eq!(p1_host::login::logout(&harness.deps, "one").await, 0);
    assert!(origins_file(root).get("one").is_none());
    // An id that is neither is a usage error naming the accounts.
    let mut harness = Harness::new(vec![root.join("environments")], &[]);
    harness.deps.home = Some(root.join("home"));
    assert_eq!(p1_host::login::logout(&harness.deps, "ghost").await, 2);
    let error = harness.stderr.text();
    assert!(
        error.contains("route `ghost` was not found") && error.contains("multi, one, two"),
        "{error}"
    );
}

#[test]
fn the_shipped_messages_routes_add_no_account_of_their_own() {
    let accounts = p1_host::routes::load_all_accounts(&[shipped_environments()]).unwrap();
    let ids: Vec<&str> = accounts.iter().map(|account| account.id.as_str()).collect();
    // ADR-0139 §9: the Go accounts are account files; their old route ids are legacy ids.
    assert!(ids.contains(&"opencode-go-2"), "{ids:?}");
    assert!(
        !ids.iter().any(|id| id.starts_with("opencode-go-messages")),
        "{ids:?}"
    );
    // Login through a Messages route id still names the backing store entry (ADR-0134).
    let account =
        p1_host::routes::load_account_by_id(&[shipped_environments()], "opencode-go-messages-2")
            .unwrap();
    assert_eq!(account.store_id, "opencode-go-2-subscription");
}

#[tokio::test]
async fn a_shipped_credential_is_approved_only_for_its_compiled_origins() {
    let dir = scratch();
    let root = dir.path();
    let compiled = p1_host::routes::shipped_origins()["glm-subscription"][0].clone();
    // A user account with a shipped id that declares one more origin (ADR-0139 §2: trust
    // cannot extend a shipped credential).
    write(
        root.join("accounts/glm-subscription.toml"),
        &format!(
            "id = \"glm-subscription\"\norigins = [\"{compiled}\", \"https://other.example.test\"]\n\
             [credential]\nmethod = \"api-key\"\nenv = \"GLM_TEST_KEY\"\nstore_only = true\n"
        ),
    );
    let mut harness = Harness::new(vec![root.join("environments")], &[]);
    harness.deps.home = Some(root.join("home"));
    harness.deps.shell_env = Some(Vec::new());
    assert_eq!(
        p1_host::login::trust_endpoint(&harness.deps, "glm-subscription").await,
        0,
        "{}",
        harness.stderr.text()
    );
    // One approved origin keeps the single-string record (acceptance condition 1).
    assert_eq!(
        origins_file(root)["glm-subscription"],
        serde_json::json!(compiled)
    );
    let said = harness.stdout.text();
    assert!(
        said.contains(&format!("trusted endpoint {compiled} for glm-subscription"))
            && said.contains("not approved: https://other.example.test"),
        "{said}"
    );
    // An account file's login names every origin it approved.
    let mut harness = Harness::new(vec![root.join("environments")], &["FAKE-STORED"]);
    harness.deps.home = Some(root.join("home"));
    harness.deps.shell_env = Some(Vec::new());
    assert_eq!(
        p1_host::login::login_with(&harness.deps, "two", false, &NoEcho).await,
        0,
        "{}",
        harness.stderr.text()
    );
    assert!(
        harness
            .stdout
            .text()
            .contains(&format!("approved origins: {ORIGIN}")),
        "{}",
        harness.stdout.text()
    );
}

/// Run one turn with `args` and return the exit code, the requests' authorization
/// headers and stderr.
async fn run_with(root: &Path, args: &[&str]) -> (i32, Vec<String>, String) {
    let mut harness = harness(root);
    let transport = Arc::new(ScriptedTransport::new(vec![ScriptedResponse::ok_sse(SSE)]));
    harness.deps.transport = transport.clone();
    let mut all: Vec<&str> = args.to_vec();
    all.extend(["--workspace", root.to_str().unwrap()]);
    let code = run_args(&mut harness, &all).await;
    let keys = transport
        .requests()
        .iter()
        .filter_map(|request| {
            request
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .map(|(_, value)| value.clone())
        })
        .collect();
    (code, keys, harness.stderr.text())
}

#[tokio::test]
async fn an_account_is_selected_by_flag_or_reference_suffix_and_must_cover_the_route() {
    let dir = scratch();
    let root = dir.path();
    environment(root, "a-one", "wire-a", Some("one"));
    account(root, "far", "https://far.example.test", "FAR_KEY");
    // ADR-0139 §3 rule 2: `@account` at the end of a model reference.
    let (code, keys, stderr) = run_with(root, &["--model", "a-one/model:high@two"]).await;
    assert_eq!(
        (code, keys),
        (0, vec!["Bearer FAKE-two".to_string()]),
        "{stderr}"
    );
    // Rule 1: `--account` for the run's own selection, over the environment's account.
    let (code, keys, stderr) = run_with(root, &["--env", "a-one", "--account", "two"]).await;
    assert_eq!(
        (code, keys),
        (0, vec!["Bearer FAKE-two".to_string()]),
        "{stderr}"
    );
    // The flag and a suffix naming another account contradict each other.
    let (code, keys, stderr) =
        run_with(root, &["--account", "one", "--model", "a-one/model@two"]).await;
    assert_eq!((code, keys.len()), (2, 0), "{stderr}");
    assert!(stderr.contains("name different accounts"), "{stderr}");
    // An account that does not declare the route's origin is refused before any request.
    let (code, keys, stderr) = run_with(root, &["--model", "a-one/model@far"]).await;
    assert_eq!((code, keys.len()), (2, 0), "{stderr}");
    assert!(
        stderr.contains("cannot run with account `far`") && stderr.contains(ORIGIN),
        "{stderr}"
    );
    // Without either, the environment's own account.
    let (code, keys, stderr) = run_with(root, &["--env", "a-one"]).await;
    assert_eq!(
        (code, keys),
        (0, vec!["Bearer FAKE-one".to_string()]),
        "{stderr}"
    );
}

#[test]
fn a_model_reference_splits_off_its_account() {
    use p1_host::models::split_account;
    assert_eq!(
        split_account("e/p:high@acct-1").unwrap(),
        ("e/p:high", Some("acct-1".to_string()))
    );
    assert_eq!(split_account("e/p").unwrap(), ("e/p", None));
    assert!(split_account("e/p@bad/id").is_err());
    // An empty account, or the effort after the account, is not a reference.
    assert!(split_account("e/p@").is_err());
    let error = split_account("e/p@acct:high").unwrap_err();
    assert!(error.contains("[:effort][@account]"), "{error}");
}

#[tokio::test]
async fn a_legacy_route_id_means_its_route_with_its_account_and_keeps_its_origin() {
    let dir = scratch();
    let root = dir.path();
    write(
        root.join("accounts/two.toml"),
        &format!(
            "id = \"two\"\norigins = [\"{ORIGIN}\"]\nstore_id = \"old-a\"\n\
             [credential]\nmethod = \"api-key\"\nenv = \"TWO_KEY\"\nstore_only = true\n\
             [legacy_routes]\n\"old-a\" = \"wire-a\"\n\
             [legacy_origins]\n\"wire-a\" = \"openai-chat/old-a\"\n"
        ),
    );
    environment(root, "old", "old-a", None);
    let (url, authorization) = request_of(root, "old").await;
    assert!(url.contains("/a/"), "{url}");
    assert_eq!(authorization, "Bearer FAKE-two");
    let dirs = [root.join("environments")];
    for key in ["old-a", "old-a@two", "wire-a@two"] {
        let route = load_route_by_id(&dirs, key).unwrap();
        assert_eq!(
            (route.route.as_str(), route.account.as_str()),
            ("wire-a", "two"),
            "{key}"
        );
        // The pair keeps the exact origin and store entry its sessions recorded.
        assert_eq!(route.origin_route, "openai-chat/old-a", "{key}");
        assert_eq!(route.credential_route_id(), "old-a", "{key}");
    }
    let keys: Vec<String> = load_route_pairs(&dirs)
        .unwrap()
        .into_iter()
        .map(|route| route.id)
        .collect();
    assert!(keys.contains(&"old-a".to_string()), "{keys:?}");
    // `p1 login old-a` names the account.
    let account = p1_host::routes::load_account_by_id(&dirs, "old-a").unwrap();
    assert_eq!(account.id, "two");
    // The legacy id with another account is a load error (ADR-0139 §3 rule 3).
    let error = load_route_by_id(&dirs, "old-a@one").unwrap_err();
    assert!(error.contains("legacy route id"), "{error}");
    // A route file with the old id itself wins.
    route(root, "old-a", "/old/chat/completions", "account = \"one\"");
    let route = load_route_by_id(&dirs, "old-a").unwrap();
    assert_eq!(
        (route.route.as_str(), route.account.as_str()),
        ("old-a", "one")
    );
}

#[tokio::test]
async fn an_environment_alias_is_its_environment_with_another_account() {
    let dir = scratch();
    let root = dir.path();
    environment(root, "a-one", "wire-a", Some("one"));
    write(
        root.join("environments/a-alias/environment.toml"),
        "alias_of = \"a-one\"\naccount = \"two\"\n",
    );
    let (url, authorization) = request_of(root, "a-alias").await;
    assert!(url.contains("/a/"), "{url}");
    assert_eq!(authorization, "Bearer FAKE-two");
    let dirs = [root.join("environments")];
    let loaded = p1_assembly::load_environment("a-alias", &dirs).unwrap();
    assert_eq!(loaded.name, "a-alias");
    assert_eq!(loaded.provider, "wire-a@two");
    // An alias has no prompt of its own, adds no other key and names no alias.
    let cases = [
        (
            "with-prompt",
            "alias_of = \"a-one\"\naccount = \"two\"\n",
            true,
            "has no",
        ),
        (
            "extra",
            "alias_of = \"a-one\"\naccount = \"two\"\nprofile = \"model\"\n",
            false,
            "only `alias_of` and `account`",
        ),
        (
            "chain",
            "alias_of = \"a-alias\"\naccount = \"one\"\n",
            false,
            "an alias itself",
        ),
        (
            "path",
            "alias_of = \"../a-one\"\naccount = \"one\"\n",
            false,
            "not an environment name",
        ),
    ];
    for (name, text, prompt, expected) in cases {
        write(
            root.join(format!("environments/{name}/environment.toml")),
            text,
        );
        if prompt {
            write(root.join(format!("environments/{name}/prompt.md")), "x");
        }
        let error = p1_assembly::load_environment(name, &dirs)
            .unwrap_err()
            .to_string();
        assert!(error.contains(expected), "{name}: {error}");
    }
}

#[test]
fn the_old_spelling_of_dialect_loads_and_both_spellings_are_an_error() {
    let dir = tempfile::tempdir().unwrap();
    let file = |stem: &str, settings: &str| {
        let path = dir.path().join(format!("{stem}.toml"));
        write(
            path.clone(),
            &format!(
                "id = \"{stem}\"\norigin_route = \"anthropic-messages/{stem}\"\n\
                 adapter = \"anthropic-messages\"\nendpoint = \"https://api.anthropic.com\"\n\
                 [credential]\nmethod = \"claude-code-oauth\"\n[adapter_settings]\n{settings}\n"
            ),
        );
        path
    };
    let old = file("old", "account = \"claude-code-subscription\"");
    let route = p1_host::routes::load_route_toml(&old).unwrap();
    let table = route.adapter_settings.as_ref().unwrap();
    assert!(table.get("dialect").is_some() && table.get("account").is_none());
    let new = file("new", "dialect = \"claude-code-subscription\"");
    assert!(p1_host::routes::load_route_toml(&new).is_ok());
    let both = file(
        "both",
        "dialect = \"claude-code-subscription\"\naccount = \"claude-code-subscription\"",
    );
    let error = p1_host::routes::load_route_toml(&both).unwrap_err();
    assert!(error.contains("both `dialect`"), "{error}");
    let mut bound = p1_host::routes::load_route(&old).unwrap();
    bound.source = old.clone();
    assert!(p1_host::routes::dialect_warning(&bound).is_some());
    bound.source = new;
    assert!(p1_host::routes::dialect_warning(&bound).is_none());
}

#[test]
fn two_pairs_with_one_origin_string_and_different_credentials_are_refused() {
    let dir = scratch();
    let root = dir.path();
    // Account two claims the origin string account one's pair on wire-a records.
    write(
        root.join("accounts/two.toml"),
        &format!(
            "id = \"two\"\norigins = [\"{ORIGIN}\"]\n\
             [credential]\nmethod = \"api-key\"\nenv = \"TWO_KEY\"\nstore_only = true\n\
             [legacy_origins]\n\"wire-b\" = \"openai-chat/wire-a@one\"\n"
        ),
    );
    let dirs = [root.join("environments")];
    let error = load_route_by_id(&dirs, "wire-b@two").unwrap_err();
    assert!(error.contains("openai-chat/wire-a@one"), "{error}");
    let error = load_route_by_id(&dirs, "wire-a@one").unwrap_err();
    assert!(error.contains("give one its own `origin_route`"), "{error}");
    // Pairs that do not share a string are unaffected.
    assert!(load_route_by_id(&dirs, "wire-a@two").is_ok());
}

#[test]
fn account_files_keep_their_own_store_entries_and_the_first_legacy_claim_wins() {
    let dir = scratch();
    let root = dir.path();
    let dirs = [root.join("environments")];
    // Two account files on one store entry would share its key and approvals.
    write(
        root.join("accounts/two.toml"),
        &format!(
            "id = \"two\"\norigins = [\"{ORIGIN}\"]\nstore_id = \"one\"\n\
             [credential]\nmethod = \"api-key\"\nenv = \"TWO_KEY\"\nstore_only = true\n"
        ),
    );
    let error = load_route_by_id(&dirs, "wire-a@two").unwrap_err();
    assert!(error.contains("both use the store entry `one`"), "{error}");
    // A legacy id claimed in a higher directory wins over a lower one.
    account(root, "two", ORIGIN, "TWO_KEY");
    let user = root.join("user");
    write(
        user.join("accounts/mine.toml"),
        &format!(
            "id = \"mine\"\norigins = [\"{ORIGIN}\"]\n\
             [credential]\nmethod = \"api-key\"\nenv = \"MINE_KEY\"\nstore_only = true\n\
             [legacy_routes]\n\"old-a\" = \"wire-b\"\n"
        ),
    );
    write(
        root.join("accounts/one.toml"),
        &format!(
            "id = \"one\"\norigins = [\"{ORIGIN}\"]\n\
             [credential]\nmethod = \"api-key\"\nenv = \"ONE_KEY\"\nstore_only = true\n\
             [legacy_routes]\n\"old-a\" = \"wire-a\"\n"
        ),
    );
    std::fs::create_dir_all(user.join("environments")).unwrap();
    let layered = [user.join("environments"), root.join("environments")];
    let route = load_route_by_id(&layered, "old-a").unwrap();
    assert_eq!(
        (route.route.as_str(), route.account.as_str()),
        ("wire-b", "mine")
    );
    // Within one directory, two claims are an error.
    write(
        root.join("accounts/two.toml"),
        &format!(
            "id = \"two\"\norigins = [\"{ORIGIN}\"]\n\
             [credential]\nmethod = \"api-key\"\nenv = \"TWO_KEY\"\nstore_only = true\n\
             [legacy_routes]\n\"old-a\" = \"wire-b\"\n"
        ),
    );
    let error = load_route_by_id(&dirs, "wire-a").unwrap_err();
    assert!(error.contains("claimed by account `one`"), "{error}");
}

/// ADR-0139 §6, §9: a user copy of a former per-account route shadows the converted account
/// of the same id, and the canonical route bound to it keeps the session origin the
/// converted account recorded, so a session of the old setup still resumes.
#[test]
fn a_user_route_copy_that_shadows_an_account_keeps_its_session_origins() {
    let dir = scratch();
    let root = dir.path();
    write(
        root.join("accounts/one.toml"),
        &format!(
            "id = \"one\"\norigins = [\"{ORIGIN}\"]\n\
             [credential]\nmethod = \"api-key\"\nenv = \"ONE_KEY\"\nstore_only = true\n\
             [legacy_routes]\n\"one\" = \"wire-a\"\n\
             [legacy_origins]\n\"wire-a\" = \"openai-chat/one\"\n"
        ),
    );
    let user = root.join("user");
    route(
        &user,
        "one",
        "/a/chat/completions",
        "[credential]\nmethod = \"api-key\"\nenv = \"ONE_KEY\"\nstore_only = true\n",
    );
    std::fs::create_dir_all(user.join("environments")).unwrap();
    let layered = [user.join("environments"), root.join("environments")];
    let copy = load_route_by_id(&layered, "one").unwrap();
    let canonical = load_route_by_id(&layered, "wire-a@one").unwrap();
    assert_eq!(
        std::fs::canonicalize(&copy.account_source).unwrap(),
        std::fs::canonicalize(user.join("routes/one.toml")).unwrap()
    );
    assert_eq!(canonical.account_source, copy.account_source);
    assert_eq!(copy.origin_route, "openai-chat/one");
    assert_eq!(canonical.origin_route, "openai-chat/one");
    assert_eq!(canonical.store_id, "one");
}

/// ADR-0139 §9: every former shipped route id resolves to the same wire, endpoint,
/// replay origin, store entry, variable and method as before the conversion; the table
/// is the pre-conversion route files' own values.
#[test]
fn every_former_shipped_route_id_resolves_exactly_as_before() {
    let dirs = [shipped_environments()];
    let before = [
        (
            "anthropic-subscription",
            "anthropic-messages/claude-subscription",
            "https://api.anthropic.com",
            "anthropic-subscription",
            None,
            "claude-code-oauth",
        ),
        (
            "anthropic-subscription-2",
            "anthropic-messages/claude-subscription-2",
            "https://api.anthropic.com",
            "anthropic-subscription-2",
            None,
            "claude-code-oauth",
        ),
        (
            "cline-pass-1",
            "openai-chat/cline-pass-1",
            "https://api.cline.bot/api/v1/chat/completions",
            "cline-pass-1",
            Some("CLINE_PASS_1_API_KEY"),
            "api-key",
        ),
        (
            "cline-pass-2",
            "openai-chat/cline-pass-2",
            "https://api.cline.bot/api/v1/chat/completions",
            "cline-pass-2",
            Some("CLINE_PASS_2_API_KEY"),
            "api-key",
        ),
        (
            "glm-subscription",
            "openai-chat/glm-subscription",
            "https://api.z.ai/api/coding/paas/v4/chat/completions",
            "glm-subscription",
            Some("ZAI_API_KEY"),
            "api-key",
        ),
        (
            "kimi-coding-subscription",
            "openai-chat/kimi-coding-subscription",
            "https://api.kimi.ai/coding/v1/chat/completions",
            "kimi-coding-subscription",
            Some("KIMI_API_KEY"),
            "api-key",
        ),
        (
            "openai-codex-subscription",
            "openai-responses/codex-subscription",
            "https://chatgpt.com/backend-api",
            "openai-codex-subscription",
            None,
            "codex-oauth",
        ),
        (
            "opencode-go-1-subscription",
            "openai-chat/opencode-go-1-subscription",
            "https://opencode.ai/zen/go/v1/chat/completions",
            "opencode-go-1-subscription",
            Some("OPENCODE_GO_1_API_KEY"),
            "api-key",
        ),
        (
            "opencode-go-2-subscription",
            "openai-chat/opencode-go-2-subscription",
            "https://opencode.ai/zen/go/v1/chat/completions",
            "opencode-go-2-subscription",
            Some("OPENCODE_GO_2_API_KEY"),
            "api-key",
        ),
        (
            "opencode-go-3-subscription",
            "openai-chat/opencode-go-3-subscription",
            "https://opencode.ai/zen/go/v1/chat/completions",
            "opencode-go-3-subscription",
            Some("OPENCODE_GO_3_API_KEY"),
            "api-key",
        ),
        (
            "opencode-go-messages-1",
            "anthropic-messages/opencode-go-messages-1",
            "https://opencode.ai/zen/go",
            "opencode-go-1-subscription",
            Some("OPENCODE_GO_1_API_KEY"),
            "api-key",
        ),
        (
            "opencode-go-messages-2",
            "anthropic-messages/opencode-go-messages-2",
            "https://opencode.ai/zen/go",
            "opencode-go-2-subscription",
            Some("OPENCODE_GO_2_API_KEY"),
            "api-key",
        ),
        (
            "opencode-go-messages-3",
            "anthropic-messages/opencode-go-messages-3",
            "https://opencode.ai/zen/go",
            "opencode-go-3-subscription",
            Some("OPENCODE_GO_3_API_KEY"),
            "api-key",
        ),
        (
            "opencode-go-messages",
            "anthropic-messages/opencode-go-messages",
            "https://opencode.ai/zen/go",
            "opencode-go-subscription",
            Some("OPENCODE_API_KEY"),
            "api-key",
        ),
        (
            "opencode-go-subscription",
            "openai-chat/opencode-go-subscription",
            "https://opencode.ai/zen/go/v1/chat/completions",
            "opencode-go-subscription",
            Some("OPENCODE_API_KEY"),
            "api-key",
        ),
        (
            "opencode-zen-1",
            "openai-chat/opencode-zen-1",
            "https://opencode.ai/zen/v1/chat/completions",
            "opencode-zen-1",
            Some("OPENCODE_ZEN_1_API_KEY"),
            "api-key",
        ),
        (
            "opencode-zen-2",
            "openai-chat/opencode-zen-2",
            "https://opencode.ai/zen/v1/chat/completions",
            "opencode-zen-2",
            Some("OPENCODE_ZEN_2_API_KEY"),
            "api-key",
        ),
        (
            "opencode-zen-3",
            "openai-chat/opencode-zen-3",
            "https://opencode.ai/zen/v1/chat/completions",
            "opencode-zen-3",
            Some("OPENCODE_ZEN_3_API_KEY"),
            "api-key",
        ),
        (
            "opencode-zen-free",
            "openai-chat/opencode-zen-free",
            "https://opencode.ai/zen/v1/chat/completions",
            "opencode-zen-free",
            Some("OPENCODE_ZEN_API_KEY"),
            "api-key",
        ),
    ];
    for (id, origin, endpoint, store, env, method) in before {
        let route = load_route_by_id(&dirs, id).unwrap_or_else(|error| panic!("{id}: {error}"));
        assert_eq!(route.origin_route, origin, "{id}");
        assert_eq!(route.endpoint, endpoint, "{id}");
        assert_eq!(route.credential_route_id(), store, "{id}");
        assert_eq!(route.credential.env.as_deref(), env, "{id}");
        assert_eq!(route.credential.kind.name(), method, "{id}");
        // The wire is the converted route's, unchanged: the same settings and bindings.
        let canonical = load_route_by_id(&dirs, &route.route).unwrap();
        assert_eq!(route.adapter, canonical.adapter, "{id}");
        assert_eq!(route.adapter_settings, canonical.adapter_settings, "{id}");
        assert_eq!(route.models, canonical.models, "{id}");
        assert_eq!(route.retry_policy, canonical.retry_policy, "{id}");
    }
    // Every shipped environment, alias or not, runs the pair it ran before.
    for (environment, id) in [
        ("claude", "anthropic-subscription"),
        ("claude2", "anthropic-subscription-2"),
        ("cline", "cline-pass-1"),
        ("cline2", "cline-pass-2"),
        ("deepseek", "opencode-go-messages"),
        ("deepseek1", "opencode-go-messages-1"),
        ("deepseek2", "opencode-go-messages-2"),
        ("deepseek3", "opencode-go-messages-3"),
        ("zen", "opencode-zen-1"),
        ("zen2", "opencode-zen-2"),
        ("zen3", "opencode-zen-3"),
    ] {
        let loaded = p1_assembly::load_environment(environment, &dirs).unwrap();
        let route = load_route_by_id(&dirs, &loaded.provider).unwrap();
        let before = load_route_by_id(&dirs, id).unwrap();
        assert_eq!(
            (
                route.origin_route.as_str(),
                route.credential_route_id(),
                &route.credential
            ),
            (
                before.origin_route.as_str(),
                before.credential_route_id(),
                &before.credential
            ),
            "{environment}"
        );
        assert_eq!(loaded.name, environment);
    }
}
