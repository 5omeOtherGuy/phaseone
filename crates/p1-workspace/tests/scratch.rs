//! The run's scratch directory as a second confined root (ADR-0122, #457).
//!
//! A scratch path resolves, is written and read exactly as a workspace path,
//! against the same checks; a path under neither root is still refused with
//! `path escapes workspace`; a symlink inside the scratch directory that points
//! outside it is refused like one in the workspace; and the shared mutation
//! counter moves only for a commit under the workspace root.

use std::fs;
use std::path::Path;

use p1_workspace::{
    Change, MutationError, MutationPolicy, ObservedFiles, Workspace, WorkspaceError,
};

fn scratch_workspace(workspace_root: &Path, scratch_root: &Path) -> Workspace {
    Workspace::new(workspace_root)
        .unwrap()
        .with_scratch(scratch_root)
        .unwrap()
}

#[test]
fn scratch_paths_resolve_write_and_read_across_both_roots() {
    let workspace_dir = tempfile::tempdir().unwrap();
    let scratch_dir = tempfile::tempdir().unwrap();
    let workspace = scratch_workspace(workspace_dir.path(), scratch_dir.path());

    // A relative workspace path and an absolute scratch path both resolve.
    let workspace_file = workspace.resolve("src/a.rs").unwrap();
    assert_eq!(workspace_file, workspace.root().join("src/a.rs"));
    let scratch_file = workspace
        .resolve(scratch_dir.path().join("body.md").to_str().unwrap())
        .unwrap();
    assert_eq!(scratch_file, scratch_dir.path().join("body.md"));

    // A write into the scratch root succeeds and its bytes come back.
    let observed = ObservedFiles::new();
    workspace
        .commit(
            &[Change::write(scratch_file.to_str().unwrap(), b"PR body")],
            &observed,
            MutationPolicy::Observed,
        )
        .unwrap();
    assert_eq!(
        fs::read(scratch_dir.path().join("body.md")).unwrap(),
        b"PR body"
    );
    let snapshot = workspace
        .read(scratch_file.to_str().unwrap(), &ObservedFiles::new())
        .unwrap();
    assert_eq!(snapshot.read(0, usize::MAX), b"PR body");

    // A write into the workspace root succeeds too, and shows only there.
    workspace
        .commit(
            &[Change::write("src/a.rs", b"fn main() {}\n")],
            &observed,
            MutationPolicy::Observed,
        )
        .unwrap();
    assert_eq!(
        fs::read(workspace_dir.path().join("src/a.rs")).unwrap(),
        b"fn main() {}\n"
    );
}

#[test]
fn a_path_under_neither_root_is_still_refused() {
    let workspace_dir = tempfile::tempdir().unwrap();
    let scratch_dir = tempfile::tempdir().unwrap();
    let third = tempfile::tempdir().unwrap();
    let workspace = scratch_workspace(workspace_dir.path(), scratch_dir.path());

    let outside = third.path().join("notes.md");
    let error = workspace.resolve(outside.to_str().unwrap()).unwrap_err();
    assert!(
        matches!(&error, WorkspaceError::OutsideWorkspace { .. }),
        "{error:?}"
    );
    assert_eq!(
        error.to_string(),
        format!("path escapes workspace: {}", outside.display())
    );

    // A `..` escape from the workspace root is refused as before.
    assert!(matches!(
        workspace.resolve("../notes.md"),
        Err(WorkspaceError::OutsideWorkspace { .. })
    ));

    // A write aimed at the third directory is refused, not created.
    let error = workspace
        .commit(
            &[Change::write(outside.to_str().unwrap(), b"body")],
            &ObservedFiles::new(),
            MutationPolicy::Observed,
        )
        .unwrap_err();
    assert!(
        matches!(&error, MutationError::OutsideWorkspace { .. }),
        "{error:?}"
    );
    assert!(!outside.exists());
}

#[cfg(unix)]
#[test]
fn a_symlink_inside_either_root_that_points_outside_is_refused() {
    use std::os::unix::fs::symlink;
    let workspace_dir = tempfile::tempdir().unwrap();
    let scratch_dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
    symlink(outside.path(), workspace_dir.path().join("link")).unwrap();
    symlink(outside.path(), scratch_dir.path().join("link")).unwrap();
    let workspace = scratch_workspace(workspace_dir.path(), scratch_dir.path());

    // The workspace link, then the scratch link: both refuse the outward escape.
    for link in [
        workspace_dir.path().join("link"),
        scratch_dir.path().join("link"),
    ] {
        let error = workspace
            .resolve(link.join("secret.txt").to_str().unwrap())
            .unwrap_err();
        assert!(
            matches!(&error, WorkspaceError::OutsideWorkspace { .. }),
            "{error:?}"
        );
        let error = workspace
            .commit(
                &[Change::write(link.join("new.txt").to_str().unwrap(), b"x")],
                &ObservedFiles::new(),
                MutationPolicy::Observed,
            )
            .unwrap_err();
        assert!(
            matches!(&error, MutationError::OutsideWorkspace { .. }),
            "{error:?}"
        );
    }
    assert!(!outside.path().join("new.txt").exists());
}

#[test]
fn the_mutation_counter_moves_only_for_the_workspace_root() {
    let workspace_dir = tempfile::tempdir().unwrap();
    let scratch_dir = tempfile::tempdir().unwrap();
    let workspace = scratch_workspace(workspace_dir.path(), scratch_dir.path());
    let observed = ObservedFiles::new();

    assert_eq!(workspace.mutations().count(), 0);

    // A scratch write bumps nothing.
    workspace
        .commit(
            &[Change::write(
                scratch_dir.path().join("body.md").to_str().unwrap(),
                b"PR body",
            )],
            &observed,
            MutationPolicy::Observed,
        )
        .unwrap();
    assert_eq!(workspace.mutations().count(), 0);

    // A workspace write bumps the counter.
    workspace
        .commit(
            &[Change::write("notes.md", b"notes")],
            &observed,
            MutationPolicy::Observed,
        )
        .unwrap();
    assert_eq!(workspace.mutations().count(), 1);
}

#[test]
fn writable_grants_preserve_observation_guards_and_workspace_accounting() {
    let container = tempfile::tempdir().unwrap();
    let root = container.path().join("ws");
    let out = container.path().join("out");
    let second = container.path().join("second");
    let nested = root.join("nested");
    for dir in [&root, &out, &second, &nested] {
        fs::create_dir_all(dir).unwrap();
    }
    let workspace = Workspace::new(&root)
        .unwrap()
        .with_writable_roots(&[
            out.clone(),
            second.clone(),
            nested,
            container.path().join("missing"),
        ])
        .unwrap();
    assert!(workspace.resolve("../missing/x").is_err());
    let observed = ObservedFiles::new();
    let file = out.join("existing.txt");
    let request = file.to_str().unwrap();
    fs::write(&file, b"old").unwrap();
    assert!(
        workspace
            .commit(
                &[Change::write(request, b"new")],
                &observed,
                MutationPolicy::Observed
            )
            .unwrap_err()
            .to_string()
            .contains("must read")
    );
    workspace.read(request, &observed).unwrap();
    fs::write(&file, b"external change").unwrap();
    assert!(
        workspace
            .commit(
                &[Change::write(request, b"new")],
                &observed,
                MutationPolicy::Observed
            )
            .unwrap_err()
            .to_string()
            .contains("changed on disk")
    );
    assert_eq!(fs::read(&file).unwrap(), b"external change");
    workspace.read(request, &observed).unwrap();
    workspace
        .commit(
            &[
                Change::write("../out/existing.txt", b"new"),
                Change::write(second.join("new.txt").to_str().unwrap(), b"second"),
            ],
            &observed,
            MutationPolicy::Observed,
        )
        .unwrap();
    assert_eq!(fs::read(file).unwrap(), b"new");
    assert_eq!(fs::read(second.join("new.txt")).unwrap(), b"second");
    assert_eq!(workspace.mutations().count(), 0);
    workspace
        .commit(
            &[Change::write("nested/new.txt", b"workspace")],
            &observed,
            MutationPolicy::Observed,
        )
        .unwrap();
    assert_eq!(workspace.mutations().count(), 1);
}

#[cfg(unix)]
#[test]
fn writable_grants_refuse_dotdot_and_symlink_escapes() {
    let container = tempfile::tempdir().unwrap();
    let root = container.path().join("ws");
    let out = container.path().join("out");
    let denied = container.path().join("denied");
    for dir in [&root, &out, &denied] {
        fs::create_dir(dir).unwrap();
    }
    std::os::unix::fs::symlink(&denied, out.join("link")).unwrap();
    let workspace = Workspace::new(&root)
        .unwrap()
        .with_writable_roots(std::slice::from_ref(&out))
        .unwrap();
    for path in [out.join("../denied/x"), out.join("link/x")] {
        let error = workspace.resolve(path.to_str().unwrap()).unwrap_err();
        assert!(matches!(error, WorkspaceError::OutsideWorkspace { .. }));
        assert!(error.to_string().contains(out.to_str().unwrap()));
        assert!(
            workspace
                .commit(
                    &[Change::write(path.to_str().unwrap(), b"no")],
                    &ObservedFiles::new(),
                    MutationPolicy::Observed
                )
                .is_err()
        );
    }
    assert!(!denied.join("x").exists());
    let no_grant = Workspace::new(&root).unwrap();
    assert!(no_grant.resolve(out.join("x").to_str().unwrap()).is_err());
}
