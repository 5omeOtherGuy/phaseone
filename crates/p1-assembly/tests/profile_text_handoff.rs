use std::sync::{Arc, Mutex};

use p1_assembly::{Catalog, Substitutions, assemble, load_environment};
use p1_contracts::Provider;

#[test]
fn assembly_hands_provider_the_exact_profile_text_it_parsed() {
    let root = tempfile::tempdir().unwrap();
    let environments = root.path().join("environments");
    let selected = environments.join("selected");
    std::fs::create_dir_all(&selected).unwrap();
    std::fs::create_dir_all(root.path().join("profiles")).unwrap();
    std::fs::write(
        selected.join("environment.toml"),
        "route = \"r\"\nprofile = \"m\"\n",
    )
    .unwrap();
    std::fs::write(selected.join("prompt.md"), "hello").unwrap();
    let path = root.path().join("profiles/m.toml");
    let text = "# original comment\nid = \"m\"\nrevision = 1\nmodel_id = \"m\"\nfamily = \"test\"\nthinking = \"enabled\"\nefforts = [\"high\"]\n";
    std::fs::write(&path, text).unwrap();
    let environment = load_environment("selected", &[environments]).unwrap();
    std::fs::write(&path, "changed after parsing").unwrap();

    let observed = Arc::new(Mutex::new(None));
    let target = observed.clone();
    let mut catalog = Catalog::new();
    catalog.provider(
        "r",
        Box::new(move |spec| {
            *target.lock().unwrap() = spec.profile_text.clone();
            Ok(Arc::new(p1_testkit::ScriptedProvider::new(vec![])) as Arc<dyn Provider>)
        }),
    );
    let substitutions = Substitutions {
        workspace: root.path().display().to_string(),
        date: "2026-01-01".into(),
        os: "test".into(),
    };
    assemble(&catalog, &environment, root.path(), &substitutions).unwrap();
    assert_eq!(observed.lock().unwrap().as_deref(), Some(text));
}
