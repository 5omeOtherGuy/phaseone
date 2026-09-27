//! The file policy that is not confinement: the credential files every file tool refuses
//! BEFORE confinement (issue #142), and the model-facing texts a refusal and a failed read
//! carry.
//!
//! The policy lives here, in the crate the native tools and the capability services a module
//! is linked with both build on, so a component can never reach what a native tool would not:
//! the refusal is compared on the canonicalised form, so a symlink or a relative path cannot
//! slip past, and it comes before any confinement error, so the model is told why and never
//! sees the file's bytes. `p1-read-guest` keeps its own copy of the two texts for the guest
//! side, which cannot depend on this crate (it builds for `wasm32-unknown-unknown`); the
//! crates that see both sides pin them equal
//! (`crates/p1-tool-read/src/lib.rs`, `the_native_texts_are_the_guests`).

use std::path::{Path, PathBuf};

use crate::Workspace;

/// Whether `candidate` is one of the credential files a file tool always refuses: the p1
/// auth store, anything under `~/.config/keys/`, and the other tools' auth files. Compared
/// on the canonicalised form, so a symlink or a relative path cannot slip past. An empty
/// `home` refuses only the XDG-named stores.
pub fn refuses_credentials(
    candidate: &Path,
    home: Option<&Path>,
    xdg_credentials: &[PathBuf],
) -> bool {
    let candidate = canonical_best_effort(candidate);
    if xdg_credentials
        .iter()
        .any(|path| canonical_best_effort(path) == candidate)
    {
        return true;
    }
    let Some(home) = home else {
        return false;
    };
    let home = canonical_best_effort(home);
    let keys = canonical_best_effort(&home.join(".config").join("keys"));
    if candidate == keys || candidate.starts_with(&keys) {
        return true;
    }
    [
        home.join(".config/p1/auth.json"),
        home.join(".codex/auth.json"),
        home.join(".claude/.credentials.json"),
        home.join(".local/share/opencode/auth.json"),
        home.join(".pi/agent/auth.json"),
    ]
    .iter()
    .any(|path| canonical_best_effort(path) == candidate)
}

/// The refusal of a credential file, before confinement, as the model is told it: the path
/// as the workspace displays it, and why. The component and the native tool show this exact
/// text.
pub fn credential_refusal(display: &str) -> String {
    format!(
        "read refuses credential files ({display}); credentials never enter the model's context"
    )
}

/// A filesystem failure while reading, as the host words `error`.
pub fn could_not_be_read(display: &str, error: &str) -> String {
    format!("{display} could not be read: {error}")
}

/// The refusal of a credential file for one request, or `Ok(())` when the path is an
/// ordinary one. `requested` is the model's own path, relative to the workspace or absolute;
/// the refusal comes first, so a credential file outside the workspace is named as one rather
/// than as a confinement error.
pub fn refuse_credentials(
    workspace: &Workspace,
    requested: &str,
    home: Option<&Path>,
    xdg_credentials: &[PathBuf],
) -> Result<(), String> {
    let candidate = if Path::new(requested).is_absolute() {
        PathBuf::from(requested)
    } else {
        workspace.root().join(requested)
    };
    if refuses_credentials(&candidate, home, xdg_credentials) {
        return Err(credential_refusal(&workspace.display(&candidate)));
    }
    Ok(())
}

/// The p1 and OpenCode stores move with their XDG override (p1-auth); a home-based path
/// below covers the default. `None` when the variable is unset.
pub fn xdg_credentials() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(config) = env_path("XDG_CONFIG_HOME") {
        files.push(config.join("p1/auth.json"));
    }
    if let Some(data) = env_path("XDG_DATA_HOME") {
        files.push(data.join("opencode/auth.json"));
    }
    files
}

/// A non-empty environment variable as a path, or `None`.
fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

/// The canonical form of `path`, or — when it does not exist yet — its canonical parent with
/// the file name appended, so a refusal never falls back to a lexical comparison against the
/// whole path.
fn canonical_best_effort(path: &Path) -> PathBuf {
    if let Ok(canonical) = path.canonicalize() {
        return canonical;
    }
    match (path.parent(), path.file_name()) {
        (Some(parent), Some(name)) => match parent.canonicalize() {
            Ok(parent) => parent.join(name),
            Err(_) => path.to_path_buf(),
        },
        _ => path.to_path_buf(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The credential files a tool refuses, relative to the home it was given.
    const CREDENTIAL_PATHS: [&str; 7] = [
        ".config/p1/auth.json",
        ".config/keys/tool.key",
        ".config/keys/nested/deeper.key",
        ".codex/auth.json",
        ".claude/.credentials.json",
        ".local/share/opencode/auth.json",
        ".pi/agent/auth.json",
    ];

    /// A temp home holding every refused credential file, as a real installation does.
    fn home_with_credentials() -> tempfile::TempDir {
        let home = tempfile::tempdir().unwrap();
        for relative in CREDENTIAL_PATHS {
            let path = home.path().join(relative);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(&path, "{}\n").unwrap();
        }
        home
    }

    #[test]
    fn every_credential_path_is_refused_and_an_ordinary_file_is_not() {
        let home = home_with_credentials();
        let home_path = home.path();
        std::fs::write(home_path.join("notes.txt"), "alpha\n").unwrap();

        for relative in CREDENTIAL_PATHS {
            let candidate = home_path.join(relative);
            assert!(
                refuses_credentials(&candidate, Some(home_path), &[]),
                "{relative}"
            );
        }
        // Everything under the keys directory is refused, its own directories included: a
        // listing of it would leak the same names.
        for relative in [".config/keys", ".config/keys/nested"] {
            assert!(
                refuses_credentials(&home_path.join(relative), Some(home_path), &[]),
                "{relative}"
            );
        }
        assert!(!refuses_credentials(
            &home_path.join("notes.txt"),
            Some(home_path),
            &[]
        ));
        // A sibling of a refused file is not refused.
        assert!(!refuses_credentials(
            &home_path.join(".codex/other.json"),
            Some(home_path),
            &[]
        ));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlink_or_a_relative_path_cannot_slip_past_the_refusal() {
        use std::os::unix::fs::symlink;

        let home = home_with_credentials();
        let home_path = home.path();
        let link = home_path.join("auth-link.json");
        symlink(home_path.join(".codex/auth.json"), &link).unwrap();

        // The link names a refused file once resolved, and a path through a link into the
        // keys directory stays inside it.
        assert!(refuses_credentials(&link, Some(home_path), &[]));
        let keys_link = home_path.join("keys-link");
        symlink(home_path.join(".config/keys"), &keys_link).unwrap();
        assert!(refuses_credentials(
            &keys_link.join("tool.key"),
            Some(home_path),
            &[]
        ));
        // A path that does not exist yet is compared through its canonical parent, so a
        // refused NAME with a `..` in front is still the refused file.
        assert!(refuses_credentials(
            &home_path.join(".config/../.codex/auth.json"),
            Some(home_path),
            &[]
        ));
    }

    #[test]
    fn an_empty_home_refuses_only_the_xdg_named_stores() {
        let home = home_with_credentials();
        let xdg = vec![home.path().join("xdg/p1/auth.json")];

        assert!(refuses_credentials(&xdg[0], None, &xdg));
        assert!(!refuses_credentials(
            &home.path().join(".codex/auth.json"),
            None,
            &xdg
        ));
    }

    #[test]
    fn the_refusal_names_the_workspace_relative_path_and_precedes_confinement() {
        let home = home_with_credentials();
        let elsewhere = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(elsewhere.path()).unwrap();

        // Inside the workspace: refused with the display form.
        assert_eq!(
            refuse_credentials(&workspace, "notes.txt", Some(home.path()), &[]),
            Ok(())
        );
        // Outside it: the credential refusal comes first and names the rule, never the
        // confinement error.
        let absolute = home.path().join(".codex/auth.json");
        let refusal = refuse_credentials(
            &workspace,
            absolute.to_str().unwrap(),
            Some(home.path()),
            &[],
        )
        .expect_err("the credential file is refused");
        assert!(
            refusal.contains("read refuses credential files"),
            "{refusal}"
        );
        assert!(
            refusal.contains("credentials never enter the model's context"),
            "{refusal}"
        );
        assert!(
            !refusal.contains("escapes workspace"),
            "the credential rule comes first: {refusal}"
        );
    }

    #[test]
    fn the_two_texts_are_the_model_facing_wording() {
        assert_eq!(
            credential_refusal("a.txt"),
            "read refuses credential files (a.txt); credentials never enter the model's context"
        );
        assert_eq!(
            could_not_be_read("a.txt", "Is a directory (os error 21)"),
            "a.txt could not be read: Is a directory (os error 21)"
        );
    }
}
