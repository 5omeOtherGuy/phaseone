//! The environment provider forms of `docs/design/routes-and-profiles.md` §1.3:
//! `route` + `profile` XOR `provider` + `model` + `family`, every other combination
//! an error naming both, the profile loaded from `profiles/<id>.toml` next to the
//! environments directory, and `assemble` handing it to the factory unchanged.

mod common;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use common::*;
use p1_assembly::{AssemblyError, Catalog, ProviderSpec, assemble, load_environment};
use p1_contracts::Provider;
use p1_testkit::ScriptedProvider;

const PROFILE_ID: &str = "example-model";
const ROUTE_ID: &str = "example-route";

/// A profile file with the given id, family and model identity.
fn profile_toml(id: &str, family: &str) -> String {
    format!(
        "id             = \"{id}\"\n\
         revision       = 2\n\
         model_id       = \"{id}-wire\"\n\
         family         = \"{family}\"\n\
         thinking       = \"enabled\"\n\
         efforts        = [\"high\", \"max\"]\n\
         default_effort = \"high\"\n\
         max_output_tokens = 4096\n"
    )
}

/// `<root>/environments` plus `<root>/profiles`, the layout `load_environment` reads.
fn root_with_env_and_profiles(root: &Path) -> PathBuf {
    std::fs::create_dir_all(root.join("profiles")).unwrap();
    let environments = root.join("environments");
    std::fs::create_dir_all(&environments).unwrap();
    environments
}

fn write_profile(root: &Path, id: &str, text: &str) {
    std::fs::write(root.join("profiles").join(format!("{id}.toml")), text).unwrap();
}

// ------------------------------------------------------ the two valid rows

#[test]
fn the_new_form_takes_the_model_and_family_from_the_profile() {
    let root = tempfile::tempdir().unwrap();
    let environments = root_with_env_and_profiles(root.path());
    write_profile(
        root.path(),
        PROFILE_ID,
        &profile_toml(PROFILE_ID, "example"),
    );
    write_environment(
        &environments,
        "routed",
        &format!("route = \"{ROUTE_ID}\"\nprofile = \"{PROFILE_ID}\"\n"),
        "prompt",
    );

    let environment = load_environment("routed", &[environments]).unwrap();
    assert_eq!(environment.provider, ROUTE_ID);
    assert_eq!(environment.model, format!("{PROFILE_ID}-wire"));
    assert_eq!(environment.family, "example");
    let profile = environment.profile.expect("the new form carries a profile");
    assert_eq!(profile.id, PROFILE_ID);
    assert_eq!(profile.revision, 2);
    assert_eq!(profile.model_id, format!("{PROFILE_ID}-wire"));
    assert_eq!(profile.family, "example");
}

#[test]
fn the_old_form_loads_a_whole_provider_and_carries_no_profile() {
    let root = tempfile::tempdir().unwrap();
    let environments = root_with_env_and_profiles(root.path());
    write_environment(
        &environments,
        "whole",
        "family = \"whole-family\"\nprovider = \"whole-provider\"\nmodel = \"whole-model\"\n",
        "prompt",
    );

    let environment = load_environment("whole", &[environments]).unwrap();
    assert_eq!(environment.provider, "whole-provider");
    assert_eq!(environment.model, "whole-model");
    assert_eq!(environment.family, "whole-family");
    assert!(environment.profile.is_none());
}

// ------------------------------------------------------ every other row

/// Every combination that is not exactly one of the two forms, with the keys it sets.
const WRONG_FORMS: &[&str] = &[
    "route = \"r\"\n",
    "profile = \"p\"\n",
    "route = \"r\"\nmodel = \"m\"\n",
    "provider = \"p\"\n",
    "provider = \"p\"\nmodel = \"m\"\n",
    "provider = \"p\"\nfamily = \"f\"\n",
    "model = \"m\"\nfamily = \"f\"\n",
    "route = \"r\"\nprofile = \"p\"\nprovider = \"p\"\n",
    "route = \"r\"\nprofile = \"p\"\nmodel = \"m\"\n",
    "route = \"r\"\nprofile = \"p\"\nfamily = \"f\"\n",
    "route = \"r\"\nprofile = \"p\"\nprovider = \"p\"\nmodel = \"m\"\nfamily = \"f\"\n",
    "provider = \"p\"\nmodel = \"m\"\nfamily = \"f\"\nprofile = \"p\"\n",
    "\n",
];

#[test]
fn every_other_key_combination_is_an_error_naming_both_forms() {
    let root = tempfile::tempdir().unwrap();
    let environments = root_with_env_and_profiles(root.path());
    write_profile(
        root.path(),
        PROFILE_ID,
        &profile_toml(PROFILE_ID, "example"),
    );

    for (index, keys) in WRONG_FORMS.iter().enumerate() {
        let name = format!("wrong{index}");
        write_environment(&environments, &name, keys, "prompt");
        let error = load_environment(&name, std::slice::from_ref(&environments)).unwrap_err();
        let message = error.to_string();
        assert!(
            matches!(error, AssemblyError::InvalidEnvironmentForm { .. }),
            "{keys:?} should be an invalid form, got {error:?}"
        );
        for form in ["`route`", "`profile`", "`provider`", "`model`", "`family`"] {
            assert!(
                message.contains(form),
                "{keys:?} must name {form} as a valid form key: {message}"
            );
        }
    }
}

#[test]
fn mixing_the_two_forms_names_the_keys_it_found() {
    let root = tempfile::tempdir().unwrap();
    let environments = root_with_env_and_profiles(root.path());
    write_profile(
        root.path(),
        PROFILE_ID,
        &profile_toml(PROFILE_ID, "example"),
    );
    write_environment(
        &environments,
        "mixed",
        &format!(
            "route = \"{ROUTE_ID}\"\nprofile = \"{PROFILE_ID}\"\n\
             family = \"example\"\nprovider = \"whole\"\nmodel = \"m\"\n"
        ),
        "prompt",
    );

    let error = load_environment("mixed", &[environments]).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("`route` + `profile`"), "{message}");
    assert!(
        message.contains("`provider` + `model` + `family`"),
        "{message}"
    );
    for found in ["`route`", "`profile`", "`provider`", "`model`", "`family`"] {
        assert!(message.contains(found), "{message}");
    }
}

// ------------------------------------------------------ the profile file

#[test]
fn a_missing_profile_names_the_profiles_that_exist() {
    let root = tempfile::tempdir().unwrap();
    let environments = root_with_env_and_profiles(root.path());
    write_profile(root.path(), "alpha", &profile_toml("alpha", "example"));
    write_profile(root.path(), "beta", &profile_toml("beta", "example"));
    std::fs::write(root.path().join("profiles/notes.txt"), "not a profile").unwrap();
    write_environment(
        &environments,
        "routed",
        &format!("route = \"{ROUTE_ID}\"\nprofile = \"{PROFILE_ID}\"\n"),
        "prompt",
    );

    let error = load_environment("routed", &[environments]).unwrap_err();
    match &error {
        AssemblyError::ProfileNotFound {
            profile, available, ..
        } => {
            assert_eq!(profile, PROFILE_ID);
            assert_eq!(available, &["alpha".to_string(), "beta".to_string()]);
        }
        other => panic!("expected ProfileNotFound, got {other:?}"),
    }
    let message = error.to_string();
    assert!(message.contains(PROFILE_ID), "{message}");
    assert!(
        message.contains("alpha") && message.contains("beta"),
        "{message}"
    );
}

#[test]
fn a_profile_file_that_is_not_valid_is_an_error_naming_the_file() {
    let root = tempfile::tempdir().unwrap();
    let environments = root_with_env_and_profiles(root.path());
    // The stem and the `id` disagree.
    write_profile(
        root.path(),
        PROFILE_ID,
        &profile_toml("other-model", "example"),
    );
    write_environment(
        &environments,
        "routed",
        &format!("route = \"{ROUTE_ID}\"\nprofile = \"{PROFILE_ID}\"\n"),
        "prompt",
    );

    let error = load_environment("routed", &[environments]).unwrap_err();
    match &error {
        AssemblyError::InvalidProfileFile { path, message } => {
            assert!(path.ends_with(format!("{PROFILE_ID}.toml")), "{path:?}");
            assert!(message.contains("other-model"), "{message}");
        }
        other => panic!("expected InvalidProfileFile, got {other:?}"),
    }
}

// ------------------------------------------------------ assemble passes it on

#[test]
fn assemble_hands_the_route_and_the_parsed_profile_to_the_factory() {
    let root = tempfile::tempdir().unwrap();
    let environments = root_with_env_and_profiles(root.path());
    write_profile(
        root.path(),
        PROFILE_ID,
        &profile_toml(PROFILE_ID, "example"),
    );

    let routed: Arc<Mutex<Vec<ProviderSpec>>> = Arc::new(Mutex::new(Vec::new()));
    let whole: Arc<Mutex<Vec<ProviderSpec>>> = Arc::new(Mutex::new(Vec::new()));
    let mut catalog = Catalog::new();
    let recorder = routed.clone();
    catalog.provider(
        ROUTE_ID,
        Box::new(move |spec: &ProviderSpec| {
            recorder.lock().unwrap().push(spec.clone());
            Ok(Arc::new(ScriptedProvider::new(Vec::new())) as Arc<dyn Provider>)
        }),
    );
    let recorder = whole.clone();
    catalog.provider(
        "whole-provider",
        Box::new(move |spec: &ProviderSpec| {
            recorder.lock().unwrap().push(spec.clone());
            Ok(Arc::new(ScriptedProvider::new(Vec::new())) as Arc<dyn Provider>)
        }),
    );
    let workspace = tempfile::tempdir().unwrap();

    write_environment(
        &environments,
        "routed",
        &format!("route = \"{ROUTE_ID}\"\nprofile = \"{PROFILE_ID}\"\n"),
        "prompt",
    );
    let environment = load_environment("routed", std::slice::from_ref(&environments)).unwrap();
    assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap();

    let routed = routed.lock().unwrap();
    assert_eq!(routed.len(), 1);
    assert_eq!(routed[0].key, ROUTE_ID);
    assert_eq!(routed[0].model, format!("{PROFILE_ID}-wire"));
    let profile = routed[0]
        .profile
        .as_ref()
        .expect("the profile is passed on");
    assert_eq!(profile.id, PROFILE_ID);
    assert_eq!(profile.family, "example");
    drop(routed);

    // The old form reaches its factory with no profile at all.
    write_environment(
        &environments,
        "whole",
        "family = \"whole-family\"\nprovider = \"whole-provider\"\nmodel = \"whole-model\"\n",
        "prompt",
    );
    let environment = load_environment("whole", &[environments]).unwrap();
    assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap();

    let whole = whole.lock().unwrap();
    assert_eq!(whole.len(), 1);
    assert_eq!(whole[0].key, "whole-provider");
    assert_eq!(whole[0].model, "whole-model");
    assert!(whole[0].profile.is_none());
}
