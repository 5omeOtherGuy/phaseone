//! The dependency rule of spec §1, enforced on the manifests: `p1-auth` names no
//! adapter, no `p1-host`, no `p1-core` and no `p1-model-profile`, and no adapter
//! names `p1-auth` back — only the host composes.

use std::path::PathBuf;

/// `(section, dependency name)` for every dependency line of a manifest.
fn dependencies(text: &str) -> Vec<(String, String)> {
    let mut section = String::new();
    let mut found = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            section = line.trim_matches(['[', ']']).to_string();
            continue;
        }
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = line.split_once('=') {
            found.push((section.clone(), name.trim().to_string()));
        }
    }
    found
}

fn manifest(relative: &str) -> String {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(relative);
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

#[test]
fn this_crate_depends_only_on_its_two_workspace_crates() {
    let text = manifest("Cargo.toml");
    assert!(
        text.contains("[dependencies]") && text.contains("[dev-dependencies]"),
        "the scan must see both dependency sections"
    );
    let allowed = ["p1-contracts", "p1-provider-http"];
    for (section, name) in dependencies(&text) {
        if name.starts_with("p1-") {
            assert!(
                allowed.contains(&name.as_str()),
                "p1-auth `[{section}]` depends on {name}; the crate owns the chain and \
                 nothing else"
            );
        }
    }
}

#[test]
fn no_adapter_depends_on_this_crate() {
    for adapter in [
        "p1-provider-anthropic",
        "p1-provider-openai",
        "p1-provider-openai-chat",
        "p1-provider-http",
        "p1-provider-conformance",
    ] {
        let text = manifest(&format!("../{adapter}/Cargo.toml"));
        for (section, name) in dependencies(&text) {
            assert_ne!(
                name, "p1-auth",
                "{adapter} `[{section}]` must not depend on p1-auth: an adapter receives \
                 `Arc<dyn CredentialSource>` and only the host composes"
            );
        }
    }
}
