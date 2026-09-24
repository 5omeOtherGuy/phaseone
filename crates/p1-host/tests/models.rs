//! Model selection at start (ADR-0049 stage 1, `docs/design/model-selection.md`
//! §1–§2): the reference rules, the glob, `settings.toml`, the scope, the
//! `p1 models` table and a headless run on a selected model.
//!
//! No test reads the real `~/.config`: every harness here points `XDG_CONFIG_HOME`
//! and `HOME` at a scratch directory, and every environment, route and profile it
//! loads is written into a tempdir (or is the repo's shipped one, read-only).
// `Settings` has a `workflows` field only with that feature; the struct updates below
// fill it with the feature on and are empty with it off.
#![cfg_attr(not(feature = "workflows"), allow(clippy::needless_update))]

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Harness, run_args, shipped_environments};
use p1_assembly::{Catalog, ProviderSpec};
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, ModelOptions, Origin, Provider, ProviderError,
    ProviderRequest, ProviderStream, RecordBody, RouteDescription,
};
use p1_host::models::{self, Model, Settings};
use p1_testkit::{ScriptedProvider, text_response};
use tempfile::TempDir;

// ------------------------------------------------------------------ fixtures

/// The shipped `environments/`, `routes/` and `profiles/`, read-only.
fn shipped() -> Vec<PathBuf> {
    vec![shipped_environments()]
}

fn shipped_models() -> Vec<Model> {
    models::enumerate(&shipped()).expect("the shipped environments, routes and profiles load")
}

const ENVIRONMENT: &str = r#"
route   = "temp-route"
profile = "p-one"

[options]
reasoning_effort = "high"

[[tools]]
module = "read"
"#;

const ROUTE: &str = r#"
id           = "temp-route"
origin_route = "openai-chat/temp"
adapter      = "openai-chat"
endpoint     = "https://example.invalid/v1/chat/completions"

[credential]
kind = "api-key"
env  = "TEMP_ROUTE_API_KEY"

[adapter_settings]
dialect = "thinking-with-reasoning-alias"

[models."p-one"]
wire_model = "wire-one"
[models."p-two"]
wire_model = "wire-two"
"#;

const PROFILE_ONE: &str = r#"
id             = "p-one"
revision       = 1
model_id       = "p-one-model"
family         = "temp"
thinking       = "enabled"
efforts        = ["low", "high"]
default_effort = "high"
"#;

const PROFILE_TWO: &str = r#"
id             = "p-two"
revision       = 1
model_id       = "p-two-model"
family         = "temp"
thinking       = "enabled"
efforts        = ["low", "medium"]
default_effort = "medium"
"#;

/// A scratch host tree: `<root>/environments`, `<root>/routes`, `<root>/profiles`,
/// plus a scratch `XDG_CONFIG_HOME`. `e-one` binds `p-one` on `temp-route`, which
/// also serves `p-two` — two models whose wire names differ from their profile ids,
/// so a run that used the wrong profile is visible in the journal.
struct Scratch {
    root: TempDir,
    config: TempDir,
}

impl Scratch {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        write(
            &root.path().join("environments/e-one/environment.toml"),
            ENVIRONMENT,
        );
        write(
            &root.path().join("environments/e-one/prompt.md"),
            "a test prompt\n",
        );
        write(&root.path().join("routes/temp-route.toml"), ROUTE);
        write(&root.path().join("profiles/p-one.toml"), PROFILE_ONE);
        write(&root.path().join("profiles/p-two.toml"), PROFILE_TWO);
        Self { root, config }
    }

    fn environment_dirs(&self) -> Vec<PathBuf> {
        vec![self.root.path().join("environments")]
    }

    fn route_file(&self) -> PathBuf {
        self.root.path().join("routes/temp-route.toml")
    }

    fn settings(&self, text: &str) {
        write(&self.config.path().join("p1/settings.toml"), text);
    }

    /// A harness whose credential locations are this scratch tree, so no test can
    /// read the real home or the real config directory.
    fn harness(&self, lines: &[&str]) -> Harness {
        let mut harness = Harness::new(self.environment_dirs(), lines);
        harness.deps.shell_env = Some(vec![
            ("HOME".into(), self.root.path().as_os_str().to_os_string()),
            (
                "XDG_CONFIG_HOME".into(),
                self.config.path().as_os_str().to_os_string(),
            ),
        ]);
        harness
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

// ------------------------------------------------------------------ the fake route

/// The fake-route mechanism the host tests already use — a catalog hook that
/// replaces a route's factory — with one addition: the fake's `describe` reports the
/// REAL route and the WIRE model the SELECTED profile binds (looked up with
/// `RouteFile::binding`, the same lookup the composed provider uses). The journal
/// and the resume check then see what a run on that model really is.
fn routed_hook(route_file: &Path, provider: ScriptedProvider) -> p1_host::catalog::CatalogHook {
    let route = p1_host::routes::load_route(route_file).expect("the test route file loads");
    let id = route.id.clone();
    Box::new(move |catalog: &mut Catalog| {
        let route = route.clone();
        let provider = provider.clone();
        catalog.provider(
            &id,
            Box::new(move |spec: &ProviderSpec| {
                let profile = spec
                    .profile
                    .clone()
                    .ok_or_else(|| "this route is reached with a profile".to_string())?;
                let binding = route.binding(&profile.id)?;
                let provider = Described {
                    inner: provider.clone(),
                    origin: Origin {
                        route: route.origin_route.clone(),
                        model: binding.wire_model.clone(),
                    },
                };
                Ok(Arc::new(provider) as Arc<dyn Provider>)
            }),
        );
    })
}

/// A [`ScriptedProvider`] that describes itself as one concrete route + wire model.
struct Described {
    inner: ScriptedProvider,
    origin: Origin,
}

impl Provider for Described {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            origin: self.origin.clone(),
            ..self.inner.describe()
        }
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.inner.validate(request)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        self.inner.stream(request, cancel)
    }
}

/// The environment record the journal opened with: the origin the core journalled
/// (route + WIRE model) and the options it was assembled with.
fn recorded(session: &Path) -> (Origin, ModelOptions) {
    let loaded = p1_journal::load(session).expect("the session loads");
    match &loaded.records[0].body {
        RecordBody::Environment { route, options, .. } => (route.origin.clone(), options.clone()),
        other => panic!("the first record is the environment: {other:?}"),
    }
}

// ------------------------------------------------------------------ enumeration

#[test]
fn every_environment_and_every_bound_profile_is_a_model() {
    let models = shipped_models();
    let ids: Vec<String> = models.iter().map(Model::id).collect();
    assert_eq!(
        ids,
        [
            "claude/claude-fable-5",
            "claude/claude-opus-4-6",
            "claude/claude-opus-5",
            "claude/claude-opus-5-5",
            "claude/claude-sonnet-4-6",
            "claude/claude-sonnet-5",
            // The two ClinePass accounts serve DeepSeek V4.1 Flash only for now (owner, 2026-09-24:
            // DeepSeek V4.1 Flash and/or GLM-5.3 Flash; GLM waits for #115).
            "cline/deepseek-v4.1-flash",
            "cline2/deepseek-v4.1-flash",
            "deepseek/deepseek-v4.1-flash",
            "deepseek1/deepseek-v4.1-flash",
            "deepseek2/deepseek-v4.1-flash",
            "deepseek3/deepseek-v4.1-flash",
            "glm/glm-5.3",
            "gpt/gpt-5.5",
            "gpt/gpt-5.6-luna",
            "gpt/gpt-5.6-sol",
            "gpt/gpt-5.6-terra",
            "gpt/gpt-6-astra",
            "gpt/gpt-6-luna",
            "gpt/gpt-6-sol",
            "kimi/kimi-k3",
            // The three free Zen accounts: every account route binds all three free models, so
            // each environment lists them and `<env>/<profile>` selects the account × model pair.
            "zen/mimo-v2.6-flash-free",
            "zen/muse-spark-1.3-contributor-free",
            "zen/space-bunny-free",
            "zen2/mimo-v2.6-flash-free",
            "zen2/muse-spark-1.3-contributor-free",
            "zen2/space-bunny-free",
            "zen3/mimo-v2.6-flash-free",
            "zen3/muse-spark-1.3-contributor-free",
            "zen3/space-bunny-free",
        ],
        "sorted by environment then profile"
    );
    // Every main agent carries the worker tools (ADR-0050), so delegation is not an
    // environment property: a model is listed once per environment, not once per
    // delegating twin.
    let opus: Vec<&Model> = models
        .iter()
        .filter(|model| model.profile == "claude-opus-5")
        .collect();
    assert_eq!(opus.len(), 1);
    assert_eq!(opus[0].environment, "claude");
    assert_eq!(opus[0].route, "anthropic-subscription");
    // The efforts are the profile's own, in its own order.
    let older = models
        .iter()
        .find(|model| model.id() == "gpt/gpt-5.5")
        .expect("the shipped gpt-5.5 profile");
    assert_eq!(older.efforts_line(), "low,medium,high,extra_high");
    let deepseek = models
        .iter()
        .find(|model| model.id() == "deepseek2/deepseek-v4.1-flash")
        .expect("the second account serves the same profile");
    assert_eq!(deepseek.efforts_line(), "high,max");
    assert_eq!(deepseek.route, "opencode-go-2-subscription");
    // The first and third Go accounts are their own routes too: same profile, same efforts,
    // different account (and therefore its own store entry, ADR-0061).
    for (id, route) in [
        (
            "deepseek1/deepseek-v4.1-flash",
            "opencode-go-1-subscription",
        ),
        (
            "deepseek3/deepseek-v4.1-flash",
            "opencode-go-3-subscription",
        ),
    ] {
        let model = models
            .iter()
            .find(|model| model.id() == id)
            .unwrap_or_else(|| panic!("the shipped {id} model"));
        assert_eq!(model.route, route, "{id}");
        assert_eq!(model.efforts_line(), "high,max", "{id}");
    }
    // Every Zen account serves every free model, and each free profile keeps its own efforts.
    for environment in ["zen", "zen2", "zen3"] {
        let served: Vec<&str> = models
            .iter()
            .filter(|model| model.environment == environment)
            .map(|model| model.profile.as_str())
            .collect();
        assert_eq!(
            served,
            [
                "mimo-v2.6-flash-free",
                "muse-spark-1.3-contributor-free",
                "space-bunny-free"
            ],
            "{environment}: every account binds the same free models"
        );
    }
    let muse = models
        .iter()
        .find(|model| model.id() == "zen3/muse-spark-1.3-contributor-free")
        .expect("the shipped Muse Spark profile");
    assert_eq!(muse.route, "opencode-zen-3");
    assert_eq!(muse.efforts_line(), "low,high");
    let bunny = models
        .iter()
        .find(|model| model.id() == "zen/space-bunny-free")
        .expect("the shipped Space Bunny profile");
    assert_eq!(bunny.route, "opencode-zen-1");
    assert_eq!(bunny.efforts_line(), "low,high,max");
    let mimo = models
        .iter()
        .find(|model| model.id() == "zen2/mimo-v2.6-flash-free")
        .expect("the shipped Mimo profile");
    assert_eq!(mimo.route, "opencode-zen-2");
    assert_eq!(mimo.efforts_line(), "high");
    // Each ClinePass environment spends its own account's key.
    for (id, route) in [
        ("cline/deepseek-v4.1-flash", "cline-pass-1"),
        ("cline2/deepseek-v4.1-flash", "cline-pass-2"),
    ] {
        let model = models
            .iter()
            .find(|model| model.id() == id)
            .unwrap_or_else(|| panic!("the shipped {id} model"));
        assert_eq!(model.route, route, "{id}");
        assert_eq!(model.efforts_line(), "high,max", "{id}");
    }
    let kimi = models
        .iter()
        .find(|model| model.id() == "kimi/kimi-k3")
        .expect("the shipped Kimi model");
    assert_eq!(kimi.route, "kimi-coding-subscription");
    assert_eq!(kimi.efforts_line(), "low,high,max");
}

#[test]
fn a_profile_with_no_efforts_renders_as_a_dash() {
    let model = Model {
        environment: "e".into(),
        profile: "p".into(),
        route: "r".into(),
        efforts: Vec::new(),
    };
    assert_eq!(model.efforts_line(), "-");
}

// ------------------------------------------------------------------ resolution

#[test]
fn a_pair_reference_resolves_to_that_pair() {
    let models = shipped_models();
    let resolved = models::resolve("claude/claude-opus-5", "claude", &models).unwrap();
    assert_eq!(resolved.environment, "claude");
    assert_eq!(resolved.profile, "claude-opus-5");
    assert_eq!(resolved.effort, None);

    let resolved = models::resolve("gpt/gpt-5.5:high", "claude", &models).unwrap();
    assert_eq!(resolved.environment, "gpt");
    assert_eq!(resolved.effort, Some(Effort::High));

    // The pair wins over the current environment: it is the whole reference.
    let resolved = models::resolve("deepseek2/deepseek-v4.1-flash", "claude", &models).unwrap();
    assert_eq!(resolved.environment, "deepseek2");
}

#[test]
fn a_bare_profile_prefers_the_current_environment() {
    let models = shipped_models();
    // `deepseek` and `deepseek2` both bind `deepseek-v4.1-flash`; the current
    // environment decides which one.
    for environment in ["deepseek", "deepseek2"] {
        let resolved = models::resolve("deepseek-v4.1-flash", environment, &models).unwrap();
        assert_eq!(resolved.environment, environment, "the current environment");
    }
    // Exactly one candidate needs no preference.
    let resolved = models::resolve("glm-5.3", "claude", &models).unwrap();
    assert_eq!(resolved.environment, "glm");
}

#[test]
fn every_resolution_failure_lists_the_candidates() {
    let models = shipped_models();

    // An unknown pair lists the models whose environment OR profile matches.
    let error = models::resolve("claude/nope", "claude", &models).unwrap_err();
    assert!(error.contains("unknown model `claude/nope`"), "{error}");
    assert!(error.contains("claude/claude-opus-5"), "{error}");
    assert!(error.contains("claude/claude-sonnet-5"), "{error}");
    assert!(
        !error.contains("gpt/gpt-5.6-sol"),
        "only the pairs whose environment or profile matches: {error}"
    );
    let error = models::resolve("nope/claude-opus-5", "claude", &models).unwrap_err();
    assert!(error.contains("claude/claude-opus-5"), "{error}");
    assert!(!error.contains("claude-sonnet-5"), "{error}");

    // A pair that matches nothing at all lists every model.
    let error = models::resolve("nope/nope", "claude", &models).unwrap_err();
    assert!(error.contains("gpt/gpt-5.5"), "{error}");

    // An ambiguous bare profile lists both pairs and never guesses.
    let error = models::resolve("deepseek-v4.1-flash", "glm", &models).unwrap_err();
    assert!(error.contains("deepseek/deepseek-v4.1-flash"), "{error}");
    assert!(error.contains("deepseek2/deepseek-v4.1-flash"), "{error}");
    assert!(error.contains("environment/profile"), "{error}");

    // A bare profile that is nobody's lists every model.
    let error = models::resolve("nope", "claude", &models).unwrap_err();
    assert!(error.contains("unknown model `nope`"), "{error}");
    assert!(error.contains("claude/claude-fable-5"), "{error}");
    assert!(error.contains("gpt/gpt-5.6-sol"), "{error}");

    // An effort outside the five levels names them.
    let error = models::resolve("claude/claude-opus-5:loud", "claude", &models).unwrap_err();
    assert!(error.contains("unknown effort `loud`"), "{error}");
    assert!(error.contains("extra_high"), "{error}");

    // A reference that is not `E/P` at all says how to write one.
    let error = models::resolve("claude/", "claude", &models).unwrap_err();
    assert!(error.contains("not a model reference"), "{error}");
}

// ------------------------------------------------------------------ the glob

#[test]
fn the_glob_matches_stars_and_questions_only() {
    assert!(models::glob("claude/*", "claude/claude-opus-5"));
    assert!(!models::glob("claude/*", "gpt/claude-opus-5"));
    assert!(models::glob("*/*", "claude/claude-opus-5"));
    assert!(models::glob("gpt/gpt-5.6-sol*", "gpt/gpt-5.6-sol"));
    assert!(models::glob("gpt/gpt-5.6-sol*", "gpt/gpt-5.6-sol-mini"));
    assert!(!models::glob("gpt/gpt-5.6-sol", "gpt/gpt-5.6-sol-mini"));
    assert!(models::glob("?pt/gpt-5.6-sol", "gpt/gpt-5.6-sol"));
    assert!(models::glob("claude-sonnet-?", "claude-sonnet-5"));
    assert!(!models::glob("claude-sonnet-?", "claude-sonnet-45"));
    assert!(models::glob("*", "anything/at-all"));
    assert!(models::glob("**", "anything/at-all"));
    assert!(!models::glob("claude", "claude/claude-opus-5"));
    assert!(!models::glob("claude-opus-5*", "claude-sonnet-5"));
}

#[test]
fn a_pattern_without_a_slash_matches_the_profile_part() {
    let models = shipped_models();
    let patterns = models::check_scope("claude-opus-5", &models).unwrap();
    let opus = models
        .iter()
        .filter(|model| models::in_scope(&patterns, model))
        .map(Model::id)
        .collect::<Vec<_>>();
    assert_eq!(opus, ["claude/claude-opus-5"]);

    let patterns = models::check_scope("claude/*", &models).unwrap();
    let claude = models
        .iter()
        .filter(|model| models::in_scope(&patterns, model))
        .count();
    assert_eq!(claude, 6, "only the `claude` environment");
    // An empty scope holds every model.
    assert!(models.iter().all(|model| models::in_scope(&[], model)));
}

#[test]
fn a_pattern_that_matches_nothing_is_an_error() {
    let models = shipped_models();
    let error = models::check_scope("claude/*,nope*", &models).unwrap_err();
    assert!(error.contains("--models pattern `nope*`"), "{error}");
    assert!(error.contains("claude/claude-fable-5"), "{error}");

    // A trailing comma is not an empty pattern.
    assert!(models::check_scope("claude/*,", &models).is_ok());
    // An empty value is an empty scope: every model.
    assert!(models::check_scope("", &models).unwrap().is_empty());

    // The settings scope names its own source.
    let settings = Settings {
        default_model: None,
        enabled_models: vec!["nope/*".to_string()],
        ..Settings::default()
    };
    let error = models::scope(None, &settings, &models).unwrap_err();
    assert!(error.contains("settings.toml `enabled_models`"), "{error}");
}

#[test]
fn the_flag_scope_replaces_the_settings_scope() {
    let models = shipped_models();
    let settings = Settings {
        default_model: None,
        enabled_models: vec!["gpt/*".to_string()],
        ..Settings::default()
    };
    assert_eq!(
        models::scope(None, &settings, &models).unwrap(),
        ["gpt/*".to_string()]
    );
    assert_eq!(
        models::scope(Some("deepseek2/*"), &settings, &models).unwrap(),
        ["deepseek2/*".to_string()],
        "--models replaces enabled_models"
    );
}

// ------------------------------------------------------------------ settings

#[test]
fn settings_parse_both_keys_and_an_absent_file_is_empty() {
    let config = tempfile::tempdir().unwrap();
    let locations = locations_for(config.path());
    assert_eq!(
        models::load_settings(&locations).unwrap(),
        Settings::default(),
        "an absent file is the empty settings"
    );

    write(
        &config.path().join("p1/settings.toml"),
        "default_model  = \"claude/claude-opus-5\"\nenabled_models = [\"claude/*\", \"gpt/gpt-5.6-sol*\"]\n",
    );
    let settings = models::load_settings(&locations).unwrap();
    assert_eq!(
        settings.default_model.as_deref(),
        Some("claude/claude-opus-5")
    );
    assert_eq!(settings.enabled_models, ["claude/*", "gpt/gpt-5.6-sol*"]);

    // An unknown key names the file and the key.
    write(
        &config.path().join("p1/settings.toml"),
        "defualt_model = \"claude/claude-opus-5\"\n",
    );
    let error = models::load_settings(&locations).unwrap_err();
    assert!(error.contains("settings.toml"), "{error}");
    assert!(error.contains("defualt_model"), "{error}");
    assert!(
        error.contains("default_model") || error.contains("expected"),
        "the error says what is expected: {error}"
    );
}

/// The credential locations of a scratch config directory: `XDG_CONFIG_HOME` and
/// `HOME` both point inside it, exactly as `p1-auth` reads them.
fn locations_for(config: &Path) -> p1_auth::Locations {
    p1_auth::Locations::from_environment([
        (
            std::ffi::OsString::from("HOME"),
            config.as_os_str().to_os_string(),
        ),
        (
            std::ffi::OsString::from("XDG_CONFIG_HOME"),
            config.as_os_str().to_os_string(),
        ),
    ])
}

#[test]
fn the_settings_path_sits_next_to_the_p1_store() {
    let config = tempfile::tempdir().unwrap();
    assert_eq!(
        models::settings_path(&locations_for(config.path())),
        Some(config.path().join("p1/settings.toml"))
    );
    // No home and no XDG config directory: no settings at all.
    assert_eq!(models::settings_path(&p1_auth::Locations::none()), None);
}

// ------------------------------------------------------------------ choose/apply

#[test]
fn default_model_is_the_model_a_bare_run_would_use() {
    let models = shipped_models();
    assert_eq!(
        models::default_model(&Settings::default(), &shipped(), &models).unwrap(),
        Some("claude/claude-sonnet-5".to_string()),
        "the default environment's own profile"
    );
    let settings = Settings {
        default_model: Some("gpt/gpt-5.5".to_string()),
        enabled_models: Vec::new(),
        ..Settings::default()
    };
    assert_eq!(
        models::default_model(&settings, &shipped(), &models).unwrap(),
        Some("gpt/gpt-5.5".to_string())
    );
    // A scratch tree whose default environment does not exist has no default model.
    let scratch = Scratch::new();
    assert_eq!(
        models::default_model(&Settings::default(), &scratch.environment_dirs(), &[]).unwrap(),
        None
    );
}

#[test]
fn default_model_decides_only_without_env_or_model() {
    let config = tempfile::tempdir().unwrap();
    write(
        &config.path().join("p1/settings.toml"),
        "default_model = \"gpt/gpt-5.5\"\n",
    );
    let locations = locations_for(config.path());
    let dirs = shipped();

    // Neither flag: the settings decide.
    let choice = models::choose(&dirs, &locations, None, None, None).unwrap();
    assert_eq!(choice.environment, "gpt");
    assert_eq!(choice.profile.as_deref(), Some("gpt-5.5"));

    // `--env` wins over the settings.
    let choice = models::choose(&dirs, &locations, Some("claude"), None, None).unwrap();
    assert_eq!(choice.environment, "claude");
    assert_eq!(choice.profile, None, "the environment's own profile");

    // `--model` wins over the settings too, and `--effort` rides along.
    let choice = models::choose(
        &dirs,
        &locations,
        None,
        Some("claude/claude-opus-5"),
        Some(Effort::Max),
    )
    .unwrap();
    assert_eq!(choice.environment, "claude");
    assert_eq!(choice.profile.as_deref(), Some("claude-opus-5"));
    assert_eq!(choice.effort, Some(Effort::Max));

    // `:effort` is used when `--effort` is absent, and `--effort` overrides it.
    let choice = models::choose(
        &dirs,
        &locations,
        None,
        Some("claude/claude-opus-5:low"),
        None,
    )
    .unwrap();
    assert_eq!(choice.effort, Some(Effort::Low));
    let choice = models::choose(
        &dirs,
        &locations,
        None,
        Some("claude/claude-opus-5:low"),
        Some(Effort::Max),
    )
    .unwrap();
    assert_eq!(choice.effort, Some(Effort::Max));
}

#[test]
fn an_env_that_disagrees_with_the_model_is_an_error() {
    let config = tempfile::tempdir().unwrap();
    let locations = locations_for(config.path());
    let dirs = shipped();

    let error = models::choose(
        &dirs,
        &locations,
        Some("claude"),
        Some("gpt/gpt-5.6-sol"),
        None,
    )
    .unwrap_err();
    assert!(error.contains("--env `claude`"), "{error}");
    assert!(error.contains("--model `gpt/gpt-5.6-sol`"), "{error}");

    // A bare profile bound in more than one environment is ambiguous, and the
    // current environment decides only when it binds the profile itself.
    let error = models::choose(
        &dirs,
        &locations,
        Some("gpt"),
        Some("deepseek-v4.1-flash"),
        None,
    )
    .unwrap_err();
    assert!(error.contains("more than one environment"), "{error}");

    // Agreeing forms resolve.
    let choice = models::choose(
        &dirs,
        &locations,
        Some("claude"),
        Some("claude-sonnet-5"),
        None,
    )
    .unwrap();
    assert_eq!(choice.environment, "claude");
    assert_eq!(choice.profile.as_deref(), Some("claude-sonnet-5"));
    let choice = models::choose(&dirs, &locations, Some("gpt"), Some("gpt/gpt-5.5"), None).unwrap();
    assert_eq!(choice.environment, "gpt");
}

#[test]
fn applying_a_selection_replaces_the_profile_and_the_effort_only() {
    let scratch = Scratch::new();
    let dirs = scratch.environment_dirs();
    let mut environment = p1_assembly::load_environment("e-one", &dirs).unwrap();
    assert_eq!(environment.profile.as_ref().unwrap().id, "p-one");
    assert_eq!(environment.options.reasoning_effort, Some(Effort::High));
    let tools = environment.tools.len();
    let prompt = environment.prompt_template.clone();

    // The environment's own profile, no effort: nothing changes.
    let choice = models::Choice {
        environment: "e-one".to_string(),
        profile: Some("p-one".to_string()),
        effort: None,
    };
    models::apply(&mut environment, &choice, &dirs).unwrap();
    assert_eq!(environment.profile.as_ref().unwrap().id, "p-one");
    assert_eq!(environment.options.reasoning_effort, Some(Effort::High));

    // Another profile: identity from the profile file, and the effort it does not
    // list replaced by its own default (§1 rule 3).
    let choice = models::Choice {
        environment: "e-one".to_string(),
        profile: Some("p-two".to_string()),
        effort: None,
    };
    models::apply(&mut environment, &choice, &dirs).unwrap();
    assert_eq!(environment.profile.as_ref().unwrap().id, "p-two");
    assert_eq!(environment.family, "temp");
    assert_eq!(environment.model, "p-two-model");
    assert_eq!(environment.options.reasoning_effort, Some(Effort::Medium));
    // Nothing else of the environment changed.
    assert_eq!(environment.tools.len(), tools);
    assert_eq!(environment.prompt_template, prompt);

    // An explicit effort replaces it.
    let choice = models::Choice {
        environment: "e-one".to_string(),
        profile: Some("p-two".to_string()),
        effort: Some(Effort::Low),
    };
    models::apply(&mut environment, &choice, &dirs).unwrap();
    assert_eq!(environment.options.reasoning_effort, Some(Effort::Low));
}

#[test]
fn an_effort_the_profile_does_not_list_names_the_profiles_efforts() {
    let scratch = Scratch::new();
    let dirs = scratch.environment_dirs();
    let mut environment = p1_assembly::load_environment("e-one", &dirs).unwrap();

    let choice = models::Choice {
        environment: "e-one".to_string(),
        profile: Some("p-two".to_string()),
        effort: Some(Effort::Max),
    };
    let error = models::apply(&mut environment, &choice, &dirs).unwrap_err();
    assert!(
        error.contains("profile `p-two` does not list effort `max`"),
        "{error}"
    );
    assert!(error.contains("low, medium"), "{error}");

    // A profile that is not there at all names the profiles that are.
    let choice = models::Choice {
        environment: "e-one".to_string(),
        profile: Some("nope".to_string()),
        effort: None,
    };
    let error = models::apply(&mut environment, &choice, &dirs).unwrap_err();
    assert!(error.contains("profile `nope` was not found"), "{error}");
    assert!(error.contains("p-one"), "{error}");
}

// ------------------------------------------------------------------ the table

#[test]
fn the_table_is_aligned_and_marked() {
    let models = shipped_models();
    let credential = |_route: &str| Ok("none — set SOMETHING".to_string());

    let table = models::table(&models, &[], Some("claude/claude-sonnet-5"), credential).unwrap();
    let lines: Vec<&str> = table.lines().collect();
    assert_eq!(lines.len(), models.len(), "one row per model");
    assert!(!table.contains("scoped"), "an empty scope omits the marker");
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.ends_with("default"))
            .count(),
        1,
        "exactly one model is the default: {table}"
    );
    assert!(lines[5].starts_with("claude/claude-sonnet-5"));
    assert!(lines[5].ends_with("default"));

    // Every column starts at the same offset in every row.
    let route_at = lines[0].find("anthropic-subscription").unwrap();
    let efforts_at = lines[0].find("low,medium,high,extra_high,max").unwrap();
    for line in &lines {
        assert!(
            line.split("  ").count() >= 4,
            "four columns are separated by two spaces: {line}"
        );
        assert!(
            line.len() >= route_at,
            "the route column starts at the same offset: {line}"
        );
    }
    assert_eq!(
        lines[1].find("anthropic-subscription"),
        Some(route_at),
        "the route column lines up"
    );
    assert_eq!(
        lines[1].find("low,medium,high,extra_high,max"),
        Some(efforts_at),
        "the efforts column lines up"
    );

    // A scope marks what is in it, and only that.
    let scope = models::check_scope("gpt/*", &models).unwrap();
    let table = models::table(&models, &scope, Some("claude/claude-sonnet-5"), credential).unwrap();
    let scoped: Vec<&str> = table
        .lines()
        .filter(|line| line.ends_with("scoped"))
        .collect();
    assert_eq!(scoped.len(), 7);
    assert!(scoped.iter().all(|line| line.starts_with("gpt/")));
}

// ------------------------------------------------------------------ the command

#[tokio::test]
async fn models_prints_one_aligned_row_per_shipped_model() {
    let scratch = Scratch::new();
    let mut harness = scratch.harness(&[]);
    let code = run_args(&mut harness, &["models"]).await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let stdout = harness.stdout.text();
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 2, "the scratch tree has two models");
    assert!(lines[0].starts_with("e-one/p-one"), "{stdout}");
    assert!(lines[0].contains("temp-route"), "{stdout}");
    assert!(lines[0].contains("low,high"), "{stdout}");
    assert!(
        lines[0].contains("none — set TEMP_ROUTE_API_KEY"),
        "the credential source, never a value: {stdout}"
    );
    assert!(
        !stdout.contains("scoped"),
        "an empty scope omits the marker"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.ends_with("default"))
            .count(),
        0,
        "the scratch tree has no default environment: {stdout}"
    );

    // SEARCH is a case-insensitive substring of `E/P`.
    let mut harness = scratch.harness(&[]);
    let code = run_args(&mut harness, &["models", "P-TWO"]).await;
    assert_eq!(code, 0);
    let stdout = harness.stdout.text();
    assert_eq!(stdout.lines().count(), 1);
    assert!(stdout.starts_with("e-one/p-two"), "{stdout}");

    // A SEARCH that matches nothing prints nothing and is not an error.
    let mut harness = scratch.harness(&[]);
    let code = run_args(&mut harness, &["models", "zzz"]).await;
    assert_eq!(code, 0);
    assert_eq!(harness.stdout.text(), "");
}

#[tokio::test]
async fn models_marks_the_default_and_the_scoped_models() {
    let scratch = Scratch::new();
    scratch.settings("default_model = \"e-one/p-two\"\n");
    let mut harness = scratch.harness(&[]);
    let code = run_args(&mut harness, &["models"]).await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let stdout = harness.stdout.text();
    assert!(
        stdout.contains("e-one/p-two") && stdout.lines().any(|line| line.ends_with("default")),
        "the settings' default_model is marked: {stdout}"
    );
    assert!(!stdout.contains("scoped"));

    // The scope comes from `enabled_models`, and `--models` replaces it.
    scratch.settings("enabled_models = [\"p-one\"]\n");
    let mut harness = scratch.harness(&[]);
    let code = run_args(&mut harness, &["models"]).await;
    assert_eq!(code, 0);
    let stdout = harness.stdout.text();
    assert_eq!(
        stdout
            .lines()
            .filter(|line| line.ends_with("scoped"))
            .count(),
        1,
        "a pattern without `/` matches the profile part: {stdout}"
    );
    assert!(
        stdout.lines().any(|line| line.starts_with("e-one/p-one")),
        "{stdout}"
    );

    let mut harness = scratch.harness(&[]);
    let code = run_args(&mut harness, &["models", "--models", "p-two"]).await;
    assert_eq!(code, 0);
    let stdout = harness.stdout.text();
    assert!(
        stdout
            .lines()
            .any(|line| line.starts_with("e-one/p-two") && line.ends_with("scoped")),
        "--models replaces enabled_models: {stdout}"
    );

    // A pattern that matches nothing fails, here and on a run.
    let mut harness = scratch.harness(&[]);
    let code = run_args(&mut harness, &["models", "--models", "nope*"]).await;
    assert_eq!(code, 2);
    assert!(
        harness.stderr.text().contains("--models pattern `nope*`"),
        "{}",
        harness.stderr.text()
    );
}

#[tokio::test]
async fn an_unknown_settings_key_fails_a_bare_run() {
    let scratch = Scratch::new();
    scratch.settings("defualt_model = \"e-one/p-two\"\n");
    let workspace = tempfile::tempdir().unwrap();
    let mut harness = scratch.harness(&[]);
    let code = run_args(
        &mut harness,
        &["--workspace", workspace.path().to_str().unwrap(), "go"],
    )
    .await;
    assert_eq!(code, 2, "stderr: {}", harness.stderr.text());
    let stderr = harness.stderr.text();
    assert!(stderr.contains("settings.toml"), "{stderr}");
    assert!(stderr.contains("defualt_model"), "{stderr}");
}

// ------------------------------------------------------------------ the run

#[tokio::test]
async fn a_headless_run_uses_the_selected_profiles_wire_model() {
    let scratch = Scratch::new();
    let workspace = tempfile::tempdir().unwrap();
    let session = workspace.path().join("session.jsonl");
    let provider = ScriptedProvider::new(vec![text_response("done")]);
    let mut harness = scratch.harness(&[]);
    harness.deps.catalog_hook = Some(routed_hook(&scratch.route_file(), provider.clone()));

    let code = run_args(
        &mut harness,
        &[
            "--model",
            "e-one/p-two",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(provider.requests().len(), 1, "the fake provider ran");

    let (origin, options) = recorded(&session);
    assert_eq!(
        origin.model, "wire-two",
        "the selected profile's wire model"
    );
    assert_eq!(origin.route, "openai-chat/temp");
    assert_eq!(
        options.reasoning_effort,
        Some(Effort::Medium),
        "p-two does not list the environment's `high`, so its own default applies"
    );
    assert_eq!(
        provider.requests()[0].options.reasoning_effort,
        Some(Effort::Medium),
        "the request carries the resolved effort"
    );
}

#[tokio::test]
async fn the_effort_flag_and_a_colon_effort_reach_the_request() {
    let scratch = Scratch::new();
    let workspace = tempfile::tempdir().unwrap();

    for (args, expected) in [
        (vec!["--env", "e-one", "--effort", "low"], Effort::Low),
        (vec!["--model", "e-one/p-one:low"], Effort::Low),
        (
            vec!["--model", "e-one/p-one", "--effort", "high"],
            Effort::High,
        ),
        // Without either, the environment's own effort stands when the profile lists it.
        (vec!["--env", "e-one"], Effort::High),
    ] {
        let session = workspace.path().join("session.jsonl");
        let provider = ScriptedProvider::new(vec![text_response("done")]);
        let mut harness = scratch.harness(&[]);
        harness.deps.catalog_hook = Some(routed_hook(&scratch.route_file(), provider.clone()));
        let mut full = args.clone();
        full.extend([
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "go",
        ]);
        let code = run_args(&mut harness, &full).await;
        assert_eq!(code, 0, "args {args:?}: {}", harness.stderr.text());
        let (origin, options) = recorded(&session);
        assert_eq!(origin.model, "wire-one", "args {args:?}");
        assert_eq!(options.reasoning_effort, Some(expected), "args {args:?}");
        std::fs::remove_file(&session).unwrap();
    }

    // An effort the profile does not list names the profile's efforts.
    let mut harness = scratch.harness(&[]);
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "e-one",
            "--effort",
            "max",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 2, "stderr: {}", harness.stderr.text());
    assert!(
        harness
            .stderr
            .text()
            .contains("profile `p-one` does not list effort `max`; it lists low, high"),
        "{}",
        harness.stderr.text()
    );
}

#[tokio::test]
async fn the_settings_default_model_decides_a_bare_run() {
    let scratch = Scratch::new();
    scratch.settings("default_model = \"e-one/p-two\"\n");
    let workspace = tempfile::tempdir().unwrap();
    let session = workspace.path().join("session.jsonl");
    let provider = ScriptedProvider::new(vec![text_response("one"), text_response("two")]);
    let mut harness = scratch.harness(&[]);
    harness.deps.catalog_hook = Some(routed_hook(&scratch.route_file(), provider.clone()));

    // No --env and no --model: the settings decide the environment AND the profile.
    let code = run_args(
        &mut harness,
        &[
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(recorded(&session).0.model, "wire-two");

    // `--env` ignores the settings; `--model` overrides them.
    std::fs::remove_file(&session).unwrap();
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(recorded(&session).0.model, "wire-one");
}

#[tokio::test]
async fn an_interactive_run_uses_the_selected_model() {
    let scratch = Scratch::new();
    let workspace = tempfile::tempdir().unwrap();
    let session = workspace.path().join("session.jsonl");
    let provider = ScriptedProvider::new(vec![text_response("done")]);
    let mut harness = scratch.harness(&["go"]);
    harness.deps.catalog_hook = Some(routed_hook(&scratch.route_file(), provider.clone()));

    let code = run_args(
        &mut harness,
        &[
            "--model",
            "e-one/p-two",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(provider.requests().len(), 1, "the prompt ran one turn");
    assert_eq!(recorded(&session).0.model, "wire-two");
}

/// The unchanged resolution path turns the selected profile into the route
/// binding's WIRE model (spec §2: nothing else of the environment changes).
#[test]
fn the_selected_profiles_binding_supplies_the_wire_model() {
    let scratch = Scratch::new();
    let dirs = scratch.environment_dirs();
    let mut environment = p1_assembly::load_environment("e-one", &dirs).unwrap();
    let choice = models::Choice {
        environment: "e-one".to_string(),
        profile: Some("p-two".to_string()),
        effort: None,
    };
    models::apply(&mut environment, &choice, &dirs).unwrap();
    assert_eq!(
        environment.model, "p-two-model",
        "the profile's own identity before resolution"
    );
    p1_host::catalog::resolve_environment(&mut environment, &dirs).unwrap();
    assert_eq!(
        environment.model, "wire-two",
        "the route binding's wire model, not the profile id"
    );
}

#[tokio::test]
async fn resume_onto_another_model_continues_the_session() {
    let scratch = Scratch::new();
    let workspace = tempfile::tempdir().unwrap();
    let session = workspace.path().join("session.jsonl");
    let provider = ScriptedProvider::new(vec![text_response("one"), text_response("two")]);
    let mut harness = scratch.harness(&[]);
    harness.deps.catalog_hook = Some(routed_hook(&scratch.route_file(), provider.clone()));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "one",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    // A session recorded on `wire-one` continues on `wire-two` (ADR-0049): the
    // provider accepts the recorded history, and the new model gets it.
    let code = run_args(
        &mut harness,
        &[
            "--model",
            "e-one/p-two",
            "--resume",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "two",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one request per turn");
    assert_eq!(
        requests[1].history.len(),
        3,
        "the second model sees the first turn and the new input"
    );
    let journal = std::fs::read_to_string(&session).unwrap();
    assert!(
        journal.contains("wire-two"),
        "the new environment is journalled"
    );
}

// ------------------------------------------------------------------ env show

#[tokio::test]
async fn env_show_prints_the_resolved_model_line() {
    let scratch = Scratch::new();
    let mut harness = scratch.harness(&[]);
    let code = run_args(&mut harness, &["env", "show", "e-one"]).await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let stdout = harness.stdout.text();
    let lines: Vec<&str> = stdout.lines().collect();
    assert!(lines[0].starts_with("credential  "), "{stdout}");
    assert_eq!(lines[1], "model  e-one/p-one:high", "{stdout}");
    assert!(stdout.contains("\"route\""), "{stdout}");

    // The shipped default environment, with the effort its `[options]` carries.
    let mut harness = Harness::new(shipped(), &[]);
    common::isolated_environment(&mut harness);
    let code = run_args(&mut harness, &["env", "show", "claude"]).await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let stdout = harness.stdout.text();
    assert_eq!(
        stdout.lines().nth(1),
        Some("model  claude/claude-sonnet-5:medium"),
        "{stdout}"
    );
}

#[tokio::test]
async fn env_show_against_the_shipped_dirs_names_the_shipped_model() {
    let mut harness = Harness::new(shipped(), &[]);
    common::isolated_environment(&mut harness);
    let code = run_args(&mut harness, &["env", "show", "deepseek2"]).await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let stdout = harness.stdout.text();
    assert_eq!(
        stdout.lines().nth(1),
        Some("model  deepseek2/deepseek-v4.1-flash:high"),
        "{stdout}"
    );

    let mut harness = Harness::new(shipped(), &[]);
    common::isolated_environment(&mut harness);
    let code = run_args(&mut harness, &["env", "show", "kimi"]).await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let stdout = harness.stdout.text();
    assert!(
        stdout
            .lines()
            .next()
            .unwrap_or_default()
            .starts_with("credential  ")
    );
    assert_eq!(stdout.lines().nth(1), Some("model  kimi/kimi-k3:high"));
    assert!(stdout.contains("\"model\": \"k3\""), "{stdout}");
}
