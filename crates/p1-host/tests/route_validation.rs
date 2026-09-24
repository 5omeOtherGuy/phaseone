//! Section 3 of `docs/design/routes-and-profiles.md`, through the host: the
//! native-option policy, the Anthropic output-cap conflict and the empty cache
//! key all fail ASSEMBLY (via the resolved provider's `validate`), and every
//! shipped `environments/*/environment.toml` still assembles. No network and no
//! credential is touched: provider construction is lazy.

mod common;

use std::path::Path;

use common::{Harness, run_args, shipped_environments};
use p1_contracts::CacheKeySupport;
use serde_json::Value;

/// `p1 env show NAME` is synchronous inside `run`: drive it on a fresh runtime.
fn show_env(name: &str) -> (i32, String, String) {
    let mut harness = Harness::new(vec![shipped_environments()], &[]);
    common::isolated_environment(&mut harness);
    let code = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(run_args(&mut harness, &["env", "show", name]));
    (code, harness.stdout.text(), harness.stderr.text())
}

/// Write `<root>/environments/<name>/` with `environment.toml` + `prompt.md`.
fn write_environment(root: &Path, name: &str, toml: &str) {
    let dir = root.join("environments").join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("environment.toml"), toml).unwrap();
    std::fs::write(dir.join("prompt.md"), "prompt").unwrap();
}

/// A shipped file, read from the repository root.
fn shipped(relative: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../")
        .join(relative)
}

/// A synthetic environments root whose `../profiles` and `../routes` hold the shipped
/// GLM profile and route, so a routed environment resolves exactly like a shipped one.
fn routed_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let profiles = root.path().join("profiles");
    std::fs::create_dir_all(&profiles).unwrap();
    std::fs::copy(
        shipped("profiles/glm-5.3.toml"),
        profiles.join("glm-5.3.toml"),
    )
    .unwrap();
    let routes = root.path().join("routes");
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::copy(
        shipped("routes/glm-subscription.toml"),
        routes.join("glm-subscription.toml"),
    )
    .unwrap();
    root
}

/// The same, for the shipped Messages route and the two Claude profiles these tests
/// select: an environment reaches the adapter exactly as the shipped `claude` one does.
fn claude_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let profiles = root.path().join("profiles");
    std::fs::create_dir_all(&profiles).unwrap();
    for id in ["claude-opus-4-6", "claude-sonnet-5"] {
        std::fs::copy(
            shipped(&format!("profiles/{id}.toml")),
            profiles.join(format!("{id}.toml")),
        )
        .unwrap();
    }
    let routes = root.path().join("routes");
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::copy(
        shipped("routes/anthropic-subscription.toml"),
        routes.join("anthropic-subscription.toml"),
    )
    .unwrap();
    root
}

/// The same, for the shipped Responses route and the GPT profile it serves: an
/// environment reaches the Codex adapter exactly as the shipped `gpt` one does.
fn codex_root() -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let profiles = root.path().join("profiles");
    std::fs::create_dir_all(&profiles).unwrap();
    std::fs::copy(
        shipped("profiles/gpt-5.6-sol.toml"),
        profiles.join("gpt-5.6-sol.toml"),
    )
    .unwrap();
    let routes = root.path().join("routes");
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::copy(
        shipped("routes/openai-codex-subscription.toml"),
        routes.join("openai-codex-subscription.toml"),
    )
    .unwrap();
    root
}

/// `env show` against a synthetic root: `(code, stderr)`.
fn show_in(root: &Path, name: &str) -> (i32, String) {
    let mut harness = Harness::new(vec![root.join("environments")], &[]);
    common::isolated_environment(&mut harness);
    let code = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(run_args(&mut harness, &["env", "show", name]));
    (code, harness.stderr.text())
}

// ------------------------------------------------------ the shipped environments

#[test]
fn every_shipped_environment_still_assembles() {
    // Every main agent gets the worker tools from the host (ADR-0050), so every
    // shipped environment assembles with or without the `delegation` feature.
    let shipped = [
        "claude",
        "gpt",
        "deepseek",
        "deepseek1",
        "deepseek2",
        "deepseek3",
        "glm",
        "zen",
        "zen2",
        "zen3",
    ];
    for name in shipped {
        let (code, stdout, stderr) = show_env(name);
        assert_eq!(code, 0, "{name}: {stderr}");
        let resolved: Value = common::env_show_json(&stdout);
        assert_eq!(
            resolved["environment"], name,
            "{name}: the resolved environment must be its own"
        );
    }
}

/// Point 1 through the host: each shipped route reports the truth about its own
/// cache-key consumption (read from its request builder).
#[test]
fn every_shipped_route_reports_its_own_cache_key_support() {
    let expected = [
        ("claude", CacheKeySupport::Unsupported),
        ("gpt", CacheKeySupport::Optional),
        // The OpenCode route carries the key as its session header; GLM's route
        // has no session header and takes no key at all.
        ("deepseek", CacheKeySupport::Optional),
        ("glm", CacheKeySupport::Unsupported),
    ];
    for (name, support) in expected {
        let (code, stdout, stderr) = show_env(name);
        assert_eq!(code, 0, "{name}: {stderr}");
        let resolved: Value = common::env_show_json(&stdout);
        let wire = match support {
            CacheKeySupport::Unsupported => "unsupported",
            CacheKeySupport::Optional => "optional",
        };
        assert_eq!(
            resolved["route"]["cache_key"], wire,
            "{name}: cache key support must match the request builder"
        );
    }
}

// ------------------------------------------------------ native options (section 3)

#[test]
fn a_native_option_in_another_adapters_namespace_fails_assembly() {
    let root = routed_root();
    write_environment(
        root.path(),
        "foreign",
        "route = \"glm-subscription\"\nprofile = \"glm-5.3\"\n\n\
         [options.native]\n\"anthropic-messages.thinking\" = true\n",
    );
    let (code, stderr) = show_in(root.path(), "foreign");
    assert_eq!(code, 1, "{stderr}");
    for part in [
        "option \"anthropic-messages.thinking\"",
        "route \"openai-chat/glm-subscription\"",
        "(adapter openai-chat)",
    ] {
        assert!(stderr.contains(part), "{stderr}: wanted `{part}`");
    }
}

#[test]
fn a_native_option_in_no_adapters_namespace_keeps_its_meaning() {
    let root = routed_root();
    write_environment(
        root.path(),
        "plain",
        "route = \"glm-subscription\"\nprofile = \"glm-5.3\"\n\n\
         [options.native]\n\"unrelated.option\" = 1\nbare = 2\n",
    );
    let (code, stderr) = show_in(root.path(), "plain");
    assert_eq!(code, 0, "{stderr}");
}

// ------------------------------------------------------ the Anthropic output cap

#[test]
fn an_explicit_output_cap_below_the_anthropic_thinking_budget_fails_assembly() {
    let root = claude_root();
    write_environment(
        root.path(),
        "capped",
        "route = \"anthropic-subscription\"\nprofile = \"claude-opus-4-6\"\n\n\
         [options]\nreasoning_effort = \"low\"\nmax_output_tokens = 4096\n",
    );
    let (code, stderr) = show_in(root.path(), "capped");
    assert_eq!(code, 1, "{stderr}");
    for part in ["max_output_tokens 4096", "thinking budget 4096", "is 4097"] {
        assert!(stderr.contains(part), "{stderr}: wanted `{part}`");
    }
}

// ------------------------------------------------------ the cache-key rules

#[test]
fn an_empty_explicit_cache_key_fails_assembly_on_the_codex_route() {
    let root = codex_root();
    write_environment(
        root.path(),
        "emptykey",
        "route = \"openai-codex-subscription\"\nprofile = \"gpt-5.6-sol\"\n\n\
         [options]\ncache_key = \"\"\n",
    );
    let (code, stderr) = show_in(root.path(), "emptykey");
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("cache_key must not be empty"), "{stderr}");
}

#[test]
fn an_explicit_cache_key_on_an_anthropic_route_fails_assembly() {
    let root = claude_root();
    write_environment(
        root.path(),
        "keyed",
        "route = \"anthropic-subscription\"\nprofile = \"claude-sonnet-5\"\n\n\
         [options]\ncache_key = \"mine\"\n",
    );
    let (code, stderr) = show_in(root.path(), "keyed");
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("takes no cache key"), "{stderr}");
}

// ------------------------------------------------------ the Responses transport

/// ADR-0047 §1 through the host: the SHIPPED route asks for
/// `transport = "websocket"` (owner decision 2026-09-21: WebSocket is the default
/// wherever a route supports it), it is composed with the host's real connector and
/// resolves exactly like an SSE one — the transport is not part of a response's
/// origin — and an unknown value fails the load before anything is composed.
#[test]
fn a_websocket_route_resolves_and_an_unknown_transport_fails_the_load() {
    let root = codex_root();
    let path = root.path().join("routes/openai-codex-subscription.toml");
    let shipped = std::fs::read_to_string(&path).unwrap();
    assert!(
        shipped.contains("transport = \"websocket\""),
        "ADR-0047 §1: the shipped route asks for WebSocket"
    );

    write_environment(
        root.path(),
        "ws",
        "route = \"openai-codex-subscription\"\nprofile = \"gpt-5.6-sol\"\n",
    );
    let (code, stderr) = show_in(root.path(), "ws");
    assert_eq!(code, 0, "{stderr}");

    std::fs::write(
        &path,
        shipped.replace("transport = \"websocket\"", "transport = \"quic\""),
    )
    .unwrap();
    let (code, stderr) = show_in(root.path(), "ws");
    assert_eq!(code, 1, "{stderr}");
    assert!(stderr.contains("invalid `[adapter_settings]`"), "{stderr}");
    assert!(stderr.contains("websocket"), "{stderr}");
}
