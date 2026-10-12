//! Kits through the host (ADR-0217): a thin environment's tables come from its family's kit,
//! and the window a kit's `[context]` leaves out is what the route serves of the model, the
//! binding's `context_limit`. Scratch catalogs and fake keys only.
mod common;

use std::path::{Path, PathBuf};

use common::{Harness, run_args};

const ORIGIN: &str = "http://127.0.0.1:9";

fn write(path: PathBuf, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A catalog with one route binding the profile `model` (family `fam`) with `binding`, the
/// `fam` kit whose `[context]` states no window, and the thin environment `thin`.
fn catalog(root: &Path, binding: &str) {
    write(
        root.join("profiles/model.toml"),
        "id = \"model\"\nrevision = 1\nmodel_id = \"model\"\nfamily = \"fam\"\n\
         thinking = \"enabled\"\nefforts = [\"high\"]\ndefault_effort = \"high\"\n",
    );
    write(
        root.join("routes/wire.toml"),
        &format!(
            "id = \"wire\"\norigin_route = \"openai-chat/wire\"\nadapter = \"openai-chat\"\n\
             endpoint = \"{ORIGIN}/v1/chat/completions\"\n[credential]\nmethod = \"api-key\"\n\
             env = \"WIRE_KEY\"\nstore_only = true\n[adapter_settings]\n\
             dialect = \"retained-thinking\"\n[models.model]\nwire_model = \"model\"\n{binding}"
        ),
    );
    write(
        root.join("kits/fam/kit.toml"),
        "prompt_file = \"prompt.md\"\n[[tools]]\nmodule = \"read\"\n[context]\n\
         output_headroom_tokens = 1000\nsummarize_at_tokens = 20000\nkeep_recent_tokens = 4000\n\
         user_verbatim_tokens = 1000\nsummary_output_tokens = 2000\n",
    );
    write(root.join("kits/fam/prompt.md"), "the fam prompt");
    write(
        root.join("environments/thin/environment.toml"),
        "route = \"wire\"\nmodel = \"model\"\n",
    );
    std::fs::create_dir_all(root.join("home")).unwrap();
}

async fn env_show(root: &Path) -> (i32, String, String) {
    let mut harness = Harness::new(vec![root.join("environments")], &[]);
    harness.deps.home = Some(root.join("home"));
    common::isolated_environment(&mut harness);
    let code = run_args(&mut harness, &["env", "show", "thin"]).await;
    (code, harness.stdout.text(), harness.stderr.text())
}

#[tokio::test]
async fn a_kit_context_without_a_window_takes_the_route_bindings_context_limit() {
    let dir = tempfile::tempdir().unwrap();
    catalog(dir.path(), "context_limit = 50000\n");
    let (code, shown, stderr) = env_show(dir.path()).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(
        shown.contains("context  window 50000 reserve 1000 summarize at 20000"),
        "{shown}"
    );
    let kit = dir.path().join("environments/../kits/fam/kit.toml");
    for table in ["context", "tools"] {
        assert!(
            shown.contains(&format!("table  {table}  {}", kit.display())),
            "{shown}"
        );
    }
    assert!(
        shown.contains("\"system_prompt\": \"the fam prompt"),
        "{shown}"
    );
}

#[tokio::test]
async fn without_a_window_anywhere_env_show_says_it_is_unknown() {
    let dir = tempfile::tempdir().unwrap();
    catalog(dir.path(), "");
    let (code, shown, stderr) = env_show(dir.path()).await;
    assert_eq!(code, 0, "{stderr}");
    assert!(shown.contains("context  window unknown"), "{shown}");
}
