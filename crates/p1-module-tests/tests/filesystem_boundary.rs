//! Adversarial filesystem suite over the workspace capability's host side (slice U-adv).
//!
//! Every case here drives the native `p1-workspace` service the `workspace`,
//! `snapshot` and `workspace-mutation` imports call — `commit`, the owned
//! `WriteGate`, `read` and `read_unobserved` — against a hostile filesystem:
//! symlinks swapped under running mutations, rename races, missing parents,
//! stale snapshots, denied roots and per-file versus multi-file atomicity. The
//! cases run on tempfile directories only, and every race is ordered with an
//! explicit barrier or by parking on the shared write gate, never with a sleep.
//!
//! Components cannot be loaded with workspace capabilities before `wasm-loader-v1`
//! (BLOCKERS.md S2-B3), so this suite pins the host side directly rather than
//! through the fixture loader; the loader-linked parity runs live in the `.2`
//! slices over S1's loader.
//!
//! Once the runtime links `workspace-mutation` (U-mut.2), the last section drives the
//! same invariants through the built `p1/write` and `p1/patch` components, loaded by
//! name through the production loader over the artifacts `scripts/build-modules.sh`
//! published, linked with the services the host links their catalog rows with
//! (`p1_host::catalog::capability_services_for`): the read side and the mutation of one
//! call share that call's read record (S2, ADR-0091).

use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::pin;
use std::sync::{Arc, Barrier};
use std::task::{Context, Poll, Waker};

use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    BoxFuture, CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
};
use p1_module_runtime::capabilities::{HeldMutation, MutationService};
use p1_module_runtime::{ExecutionLimits, Loader, ReleaseManifest, Services, wasm_tool};
use p1_module_tests::within_deadline;
use p1_redact::MaskCounter;
use p1_workspace::{
    Change, MutationError, MutationPolicy, Observation, ObservedFiles, ReadRecord, Workspace,
    WorkspaceError,
};

/// A workspace in a fresh temporary directory holding the files `(path, contents)`.
fn workspace(files: &[(&str, &str)]) -> (tempfile::TempDir, Workspace) {
    let dir = tempfile::tempdir().unwrap();
    for (path, contents) in files {
        let path = dir.path().join(path);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, contents).unwrap();
    }
    let workspace = Workspace::new(dir.path()).unwrap();
    (dir, workspace)
}

fn text(workspace: &Workspace, path: &str) -> String {
    fs::read_to_string(workspace.root().join(path)).unwrap()
}

/// Every entry under `dir`, recursively, as names relative to `dir`. A symlink is
/// listed but never descended, so a link pointing outside cannot fake entries.
fn entries(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let mut pending = vec![dir.to_path_buf()];
    while let Some(next) = pending.pop() {
        for entry in fs::read_dir(&next).unwrap() {
            let entry = entry.unwrap();
            let kind = entry.file_type().unwrap();
            found.push(
                entry
                    .path()
                    .strip_prefix(dir)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned(),
            );
            if kind.is_dir() {
                pending.push(entry.path());
            }
        }
    }
    found.sort();
    found
}

/// No staged sibling temporary (`temp_name` marks them `.p1-tmp-`) survives any case.
fn assert_no_temporaries(dir: &Path) {
    let temporaries: Vec<String> = entries(dir)
        .into_iter()
        .filter(|name| name.contains(".p1-tmp-"))
        .collect();
    assert!(
        temporaries.is_empty(),
        "staged temporaries left: {temporaries:?}"
    );
}

fn outside_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// Poll a future that must be ready at once (the gate is free), the way the host
/// acquires the owned gate from a module's `begin` when no other writer holds it.
fn ready<F: Future>(future: F) -> F::Output {
    match pin!(future).poll(&mut Context::from_waker(Waker::noop())) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the gate was expected to be free"),
    }
}

// --------------------------------------------------------------- in-root symlinks

#[cfg(unix)]
#[test]
fn an_in_root_symlink_is_followed_and_stays_inside() {
    use std::os::unix::fs::symlink;

    let (dir, ws) = workspace(&[("real/a.txt", "one\n"), ("real/keep.txt", "keep\n")]);
    symlink(dir.path().join("real"), dir.path().join("link")).unwrap();
    symlink(dir.path().join("real/a.txt"), dir.path().join("alias")).unwrap();

    // Reading through a directory link and through a file link resolves to the real
    // file, and both report the canonical path the host keys observations by.
    let via_link = ws.read("link/a.txt", &ObservedFiles::new()).unwrap();
    assert_eq!(via_link.read(0, 100), b"one\n");
    assert_eq!(
        ws.resolve("link/a.txt").unwrap(),
        ws.root().join("real/a.txt")
    );
    let via_alias = ws.read("alias", &ObservedFiles::new()).unwrap();
    assert_eq!(via_alias.read(0, 100), b"one\n");
    assert_eq!(ws.resolve("alias").unwrap(), ws.root().join("real/a.txt"));

    // A mutation named through the link lands on the real file inside, never a second
    // file, and a create through the link stays inside too.
    let observed = ObservedFiles::new();
    ws.read("link/a.txt", &observed).unwrap();
    ws.commit(
        &[Change::write("link/a.txt", "two\n")],
        &observed,
        MutationPolicy::Observed,
    )
    .unwrap();
    assert_eq!(text(&ws, "real/a.txt"), "two\n");
    ws.commit(
        &[Change::create("link/new.txt", "fresh\n")],
        &observed,
        MutationPolicy::Observed,
    )
    .unwrap();
    assert_eq!(text(&ws, "real/new.txt"), "fresh\n");
    assert_eq!(
        ws.resolve("link/new.txt").unwrap(),
        ws.root().join("real/new.txt")
    );

    assert_eq!(
        entries(dir.path()),
        vec![
            "alias".to_string(),
            "link".to_string(),
            "real".to_string(),
            "real/a.txt".to_string(),
            "real/keep.txt".to_string(),
            "real/new.txt".to_string(),
        ]
    );
    assert_no_temporaries(dir.path());
}

// ----------------------------------------------------------- escaping symlinks

#[cfg(unix)]
#[test]
fn an_escaping_symlink_is_outside_workspace_for_every_operation() {
    use std::os::unix::fs::symlink;

    let (dir, ws) = workspace(&[("inside.txt", "inside\n")]);
    let outside = outside_dir();
    fs::write(outside.path().join("victim.txt"), "outside\n").unwrap();
    // A leaf symlink to a file outside, and an ancestor symlink to a directory outside.
    symlink(outside.path().join("victim.txt"), dir.path().join("leaf")).unwrap();
    symlink(outside.path(), dir.path().join("ancestor")).unwrap();

    // Reads of the leaf, of a file through the escaping ancestor, and of a not-yet
    // existing leaf under it are all the typed confinement refusal.
    for path in [
        "leaf",
        "ancestor",
        "ancestor/victim.txt",
        "ancestor/new.txt",
    ] {
        let error = ws.read(path, &ObservedFiles::new()).unwrap_err();
        assert!(
            matches!(error, WorkspaceError::OutsideWorkspace { .. }),
            "{path}: {error:?}"
        );
        assert!(matches!(
            ws.stat(path).unwrap_err(),
            WorkspaceError::OutsideWorkspace { .. }
        ));
    }
    assert!(matches!(
        ws.list("ancestor").unwrap_err(),
        WorkspaceError::OutsideWorkspace { .. }
    ));

    // Every mutation of an escaping leaf or ancestor refuses, for write, create,
    // remove and both ends of a rename.
    let observed = ObservedFiles::new();
    ws.read("inside.txt", &observed).unwrap();
    let escaping = [
        Change::write("leaf", "pwned\n"),
        Change::create("leaf", "pwned\n"),
        Change::remove("leaf"),
        Change::rename("leaf", "moved.txt"),
        Change::write("ancestor/victim.txt", "pwned\n"),
        Change::create("ancestor/new.txt", "pwned\n"),
        Change::remove("ancestor/victim.txt"),
        Change::rename("inside.txt", "ancestor/landed.txt"),
        Change::rename("ancestor/victim.txt", "moved.txt"),
    ];
    for change in escaping {
        let result = ws.commit(
            std::slice::from_ref(&change),
            &observed,
            MutationPolicy::PatchAuthorized,
        );
        assert!(
            matches!(result, Err(MutationError::OutsideWorkspace { .. })),
            "{change:?}: {result:?}"
        );
    }

    // Nothing outside changed and nothing was created inside.
    assert_eq!(
        fs::read_to_string(outside.path().join("victim.txt")).unwrap(),
        "outside\n"
    );
    assert_eq!(entries(outside.path()), vec!["victim.txt".to_string()]);
    assert!(!dir.path().join("moved.txt").exists());
    assert_eq!(text(&ws, "inside.txt"), "inside\n");
    assert_no_temporaries(dir.path());
}

// ------------------------------------- a parent swapped between validation and apply

// The directory-relative walk that closes the replacement race (`open_parent` opens each
// ancestor from the root's handle, refusing to follow a symlink) is only observable from
// outside through `commit`'s own plan-then-gate split: the public API exposes no hook
// between the throwaway validation and the applying walk, which is why the crate's own
// unit tests reach that exact window with a private two-step helper. These cases pin what
// the public API does expose — the swap before validation, and the swap while the call is
// parked on the held write gate — and assert the invariant in every ordering: refused,
// nothing outside the root, no temporary behind.

/// A parent swapped for an escaping symlink strictly before the call: `commit`'s
/// validation resolves the target through the swapped link and refuses it. Nothing
/// lands outside the root.
#[cfg(unix)]
#[test]
fn a_parent_swapped_before_commit_is_refused() {
    use std::os::unix::fs::symlink;

    let outside = outside_dir();
    fs::write(outside.path().join("victim.txt"), "outside\n").unwrap();
    let (dir, ws) = workspace(&[("sub/victim.txt", "inside\n")]);
    let observed = ObservedFiles::new();
    ws.read("sub/victim.txt", &observed).unwrap();

    fs::rename(dir.path().join("sub"), dir.path().join("sub.real")).unwrap();
    symlink(outside.path(), dir.path().join("sub")).unwrap();

    let result = ws.commit(
        &[Change::write("sub/victim.txt", "pwned\n")],
        &observed,
        MutationPolicy::Observed,
    );
    assert!(
        matches!(result, Err(MutationError::OutsideWorkspace { .. })),
        "{result:?}"
    );
    assert_eq!(
        fs::read_to_string(outside.path().join("victim.txt")).unwrap(),
        "outside\n"
    );
    assert_eq!(entries(outside.path()), vec!["victim.txt".to_string()]);
    assert_eq!(
        fs::read_to_string(dir.path().join("sub.real/victim.txt")).unwrap(),
        "inside\n"
    );
    assert_no_temporaries(dir.path());
}

/// The native `commit` validates every target *before* it takes the gate and applies the
/// changes. Holding the gate parks the committing thread in that window, and a second
/// thread released by a barrier swaps the parent for an escaping symlink while it waits.
/// The swap races the throwaway validation's own stat/canonicalize, so the refusal can be
/// the confinement error or an I/O error naming the target; either way the mutation never
/// lands outside the root and leaves no temporary, which is the invariant this case pins.
#[cfg(unix)]
#[test]
fn a_parent_swapped_while_a_commit_waits_writes_nothing_outside() {
    use std::os::unix::fs::symlink;

    let outside = outside_dir();
    fs::write(outside.path().join("victim.txt"), "outside\n").unwrap();
    let (dir, ws) = workspace(&[("sub/victim.txt", "inside\n")]);
    let observed = ObservedFiles::new();
    ws.read("sub/victim.txt", &observed).unwrap();

    let held = ws.begin_mutation();
    let started = Arc::new(Barrier::new(2));
    let commit = {
        let ws = ws.clone();
        let observed = observed.clone();
        let started = Arc::clone(&started);
        std::thread::spawn(move || {
            started.wait();
            ws.commit(
                &[Change::write("sub/victim.txt", "pwned\n")],
                &observed,
                MutationPolicy::Observed,
            )
        })
    };
    started.wait();
    fs::rename(dir.path().join("sub"), dir.path().join("sub.real")).unwrap();
    symlink(outside.path(), dir.path().join("sub")).unwrap();
    drop(held);
    let result = commit.join().unwrap();

    // Refused, never applied: through the swapped parent the walk refuses with the
    // confinement error, and the rename racing the recheck's stat refuses as I/O.
    match &result {
        Err(MutationError::OutsideWorkspace { .. }) => {}
        Err(MutationError::Io(message)) => assert!(
            message.contains("sub/victim.txt"),
            "the refusal must name the target: {message}"
        ),
        other => panic!("a swapped parent must be refused, never applied: {other:?}"),
    }
    assert_eq!(
        fs::read_to_string(outside.path().join("victim.txt")).unwrap(),
        "outside\n"
    );
    assert_eq!(entries(outside.path()), vec!["victim.txt".to_string()]);
    assert_eq!(
        fs::read_to_string(dir.path().join("sub.real/victim.txt")).unwrap(),
        "inside\n"
    );
    assert_no_temporaries(dir.path());
}

/// The component path: it validates and computes outside the gate, then takes the owned
/// gate and writes. A swap performed between its own validation and its gated write —
/// with the owned gate ordering the steps — is refused, and never redirects the write.
#[cfg(unix)]
#[test]
fn a_swap_between_the_components_validation_and_its_gated_write_is_refused() {
    use std::os::unix::fs::symlink;

    let outside = outside_dir();
    fs::write(outside.path().join("victim.txt"), "outside\n").unwrap();
    let (dir, ws) = workspace(&[("sub/victim.txt", "inside\n")]);
    let observed = ObservedFiles::new();

    // The component validates: it reads the target and sees it inside the workspace.
    let snapshot = ws.read("sub/victim.txt", &observed).unwrap();
    assert_eq!(snapshot.read(0, 100), b"inside\n");

    // It takes the owned gate, then another agent swaps the parent for an escaping link.
    let owned = ready(ws.begin_owned(&observed, &ReadRecord::new(), MutationPolicy::Observed));
    fs::rename(dir.path().join("sub"), dir.path().join("sub.real")).unwrap();
    symlink(outside.path(), dir.path().join("sub")).unwrap();

    // The gated write re-validates under the gate and refuses the swapped parent.
    let result = owned.write("sub/victim.txt", "pwned\n");
    drop(owned);
    assert!(
        matches!(result, Err(MutationError::OutsideWorkspace { .. })),
        "{result:?}"
    );
    assert_eq!(
        fs::read_to_string(outside.path().join("victim.txt")).unwrap(),
        "outside\n"
    );
    assert_eq!(entries(outside.path()), vec!["victim.txt".to_string()]);
    assert_eq!(
        fs::read_to_string(dir.path().join("sub.real/victim.txt")).unwrap(),
        "inside\n"
    );
    assert_no_temporaries(dir.path());
}

/// The same race when the write would create missing parents: a not-yet-existing leaf
/// under a swapped parent must not create directories or a file outside the root.
#[cfg(unix)]
#[test]
fn a_swap_before_a_write_with_missing_parents_creates_nothing_outside() {
    use std::os::unix::fs::symlink;

    let outside = outside_dir();
    let (dir, ws) = workspace(&[("keep.txt", "k\n")]);
    let observed = ObservedFiles::new();
    ws.read("keep.txt", &observed).unwrap();

    let owned = ready(ws.begin_owned(&observed, &ReadRecord::new(), MutationPolicy::Observed));
    fs::rename(dir.path().join("keep.txt"), dir.path().join("keep.real")).unwrap();
    symlink(outside.path(), dir.path().join("keep.txt")).unwrap();

    // `keep.txt` is now an escaping link; a create below it must not touch the outside.
    let result = owned.create("keep.txt/deep/new.txt", "pwned\n");
    drop(owned);
    assert!(
        matches!(result, Err(MutationError::OutsideWorkspace { .. })),
        "{result:?}"
    );
    assert_eq!(entries(outside.path()), Vec::<String>::new());
    assert_no_temporaries(dir.path());
}

// ------------------------------------------------------------------ rename races

/// An ungated writer fills a rename destination while the commit waits on the gate. The
/// destination appears under the gate before the rename, so the rename is refused and
/// neither file is left in a partial state.
#[cfg(unix)]
#[test]
fn a_rename_race_refuses_a_destination_filled_while_the_commit_waits() {
    let (dir, ws) = workspace(&[("from.txt", "from\n")]);
    let observed = ObservedFiles::new();
    ws.read("from.txt", &observed).unwrap();

    let held = ws.begin_mutation();
    let started = Arc::new(Barrier::new(2));
    let commit = {
        let ws = ws.clone();
        let observed = observed.clone();
        let started = Arc::clone(&started);
        std::thread::spawn(move || {
            started.wait();
            ws.commit(
                &[Change::rename("from.txt", "to.txt")],
                &observed,
                MutationPolicy::Observed,
            )
        })
    };
    started.wait();
    fs::write(dir.path().join("to.txt"), "ungated\n").unwrap();
    drop(held);
    let result = commit.join().unwrap();

    assert!(
        matches!(result, Err(MutationError::AlreadyExists { .. })),
        "{result:?}"
    );
    assert_eq!(text(&ws, "from.txt"), "from\n");
    assert_eq!(text(&ws, "to.txt"), "ungated\n");
    assert_no_temporaries(dir.path());
}

/// A create whose target an ungated writer fills while the commit waits: `create` is
/// refused by the atomic no-replace rename, never silently overwritten.
#[cfg(unix)]
#[test]
fn a_create_race_refuses_a_target_filled_while_the_commit_waits() {
    let (dir, ws) = workspace(&[("keep.txt", "k\n")]);
    let observed = ObservedFiles::new();

    let held = ws.begin_mutation();
    let started = Arc::new(Barrier::new(2));
    let commit = {
        let ws = ws.clone();
        let observed = observed.clone();
        let started = Arc::clone(&started);
        std::thread::spawn(move || {
            started.wait();
            ws.commit(
                &[Change::create("new.txt", "from commit\n")],
                &observed,
                MutationPolicy::Observed,
            )
        })
    };
    started.wait();
    fs::write(dir.path().join("new.txt"), "ungated\n").unwrap();
    drop(held);
    let result = commit.join().unwrap();

    assert!(
        matches!(result, Err(MutationError::AlreadyExists { .. })),
        "{result:?}"
    );
    assert_eq!(text(&ws, "new.txt"), "ungated\n");
    assert_no_temporaries(dir.path());
}

/// The source is renamed away while the commit waits. A change computed from the
/// source's snapshot is refused as stale, or by the pre-gate validation when the
/// rename wins that window; the same write without a snapshot recreates the target
/// atomically, or is refused the same way. Every outcome is whole, never a partial file.
#[cfg(unix)]
#[test]
fn a_rename_race_refuses_or_applies_atomically_when_the_source_moves_away() {
    // (a) computed from a snapshot: the source moving away is staleness.
    let (dir, ws) = workspace(&[("a.txt", "one\n")]);
    let observed = ObservedFiles::new();
    let snapshot = ws.read("a.txt", &observed).unwrap().metadata();
    let held = ws.begin_mutation();
    let started = Arc::new(Barrier::new(2));
    let commit = {
        let ws = ws.clone();
        let observed = observed.clone();
        let started = Arc::clone(&started);
        std::thread::spawn(move || {
            started.wait();
            ws.commit(
                &[Change::write("a.txt", "new\n").computed_from(&snapshot)],
                &observed,
                MutationPolicy::Observed,
            )
        })
    };
    started.wait();
    fs::rename(dir.path().join("a.txt"), dir.path().join("gone.txt")).unwrap();
    drop(held);
    let result = commit.join().unwrap();
    // The rename races the whole call, not only the gated apply: if it wins the
    // pre-gate validation's own `exists()` -> `canonicalize()` window, the refusal
    // is an I/O error naming the target instead of the gated staleness check's
    // "changed on disk". Either ordering refuses whole, so both are accepted.
    assert!(
        matches!(&result, Err(MutationError::Io(message))
            if message.contains("changed on disk") || message.contains("a.txt")),
        "{result:?}"
    );
    assert!(!dir.path().join("a.txt").exists());
    assert_eq!(text(&ws, "gone.txt"), "one\n");

    // (b) no snapshot: the write recreates the target whole, or is refused whole.
    let (dir, ws) = workspace(&[("a.txt", "one\n")]);
    let observed = ObservedFiles::new();
    ws.read("a.txt", &observed).unwrap();
    let held = ws.begin_mutation();
    let started = Arc::new(Barrier::new(2));
    let commit = {
        let ws = ws.clone();
        let observed = observed.clone();
        let started = Arc::clone(&started);
        std::thread::spawn(move || {
            started.wait();
            ws.commit(
                &[Change::write("a.txt", "new\n")],
                &observed,
                MutationPolicy::Observed,
            )
        })
    };
    started.wait();
    fs::rename(dir.path().join("a.txt"), dir.path().join("gone.txt")).unwrap();
    drop(held);
    let result = commit.join().unwrap();
    // Two whole outcomes depending on which side wins the window between the
    // barrier release and the pre-gate validation: the gated apply recreates the
    // target atomically, or the rename wins the validation's
    // `exists()` -> `canonicalize()` race and the call is refused with an I/O
    // error naming a.txt. A partial file is the only outcome never accepted.
    match &result {
        Ok(()) => assert_eq!(text(&ws, "a.txt"), "new\n"),
        Err(MutationError::Io(message)) => {
            assert!(
                message.contains("a.txt"),
                "the refusal must name the target: {message}"
            );
            assert!(!dir.path().join("a.txt").exists());
        }
        other => panic!("a renamed-away source must be applied or refused whole: {other:?}"),
    }
    assert_eq!(text(&ws, "gone.txt"), "one\n");
    assert_no_temporaries(dir.path());
}

// ------------------------------------------------------------------ missing parents

#[cfg(unix)]
#[test]
fn missing_parents_are_created_inside_only() {
    use std::os::unix::fs::symlink;

    let (dir, ws) = workspace(&[("keep.txt", "k\n")]);
    let outside = outside_dir();
    let observed = ObservedFiles::new();

    // Inside: a deep missing chain is created under the root.
    ws.commit(
        &[Change::create("deep/er/new.txt", "fresh\n")],
        &observed,
        MutationPolicy::Observed,
    )
    .unwrap();
    assert_eq!(text(&ws, "deep/er/new.txt"), "fresh\n");
    assert!(ws.root().join("deep/er").is_dir());

    // Outside: a missing chain under an escaping symlink is refused, and no directory
    // or file is created at the link's target.
    symlink(outside.path(), dir.path().join("esc")).unwrap();
    let result = ws.commit(
        &[Change::create("esc/deep/new.txt", "pwned\n")],
        &observed,
        MutationPolicy::Observed,
    );
    assert!(
        matches!(result, Err(MutationError::OutsideWorkspace { .. })),
        "{result:?}"
    );
    assert_eq!(entries(outside.path()), Vec::<String>::new());

    // A chain that climbs out lexically is refused before anything is created.
    let result = ws.commit(
        &[Change::create("deep/../../etc/pwned.txt", "x")],
        &observed,
        MutationPolicy::Observed,
    );
    assert!(
        matches!(result, Err(MutationError::OutsideWorkspace { .. })),
        "{result:?}"
    );
    assert_no_temporaries(dir.path());
}

// ------------------------------------------------------------------ stale snapshots

#[test]
fn a_stale_snapshot_is_refused() {
    let (dir, ws) = workspace(&[("a.txt", "one\n")]);
    let observed = ObservedFiles::new();
    let snapshot = ws.read("a.txt", &observed).unwrap().metadata();

    // The file changed after the agent observed it: a change computed from that
    // snapshot is refused whatever the policy, including the patch exemption.
    fs::write(dir.path().join("a.txt"), "two\n").unwrap();
    for policy in [MutationPolicy::Observed, MutationPolicy::PatchAuthorized] {
        let result = ws.commit(
            &[Change::write("a.txt", "three\n").computed_from(&snapshot)],
            &observed,
            policy,
        );
        assert!(
            matches!(&result, Err(MutationError::Io(message)) if message.contains("changed on disk")),
            "{policy:?}: {result:?}"
        );
    }
    assert_eq!(text(&ws, "a.txt"), "two\n");

    // The observation alone (no snapshot) refuses the same staleness for observed mode.
    let result = ws.commit(
        &[Change::write("a.txt", "three\n")],
        &observed,
        MutationPolicy::Observed,
    );
    assert!(
        matches!(&result, Err(MutationError::Io(message)) if message.contains("changed on disk")),
        "{result:?}"
    );
    assert_eq!(text(&ws, "a.txt"), "two\n");

    // Re-reading re-observes the current bytes, and the commit applies.
    let current = ws.read("a.txt", &observed).unwrap().metadata();
    ws.commit(
        &[Change::write("a.txt", "three\n").computed_from(&current)],
        &observed,
        MutationPolicy::Observed,
    )
    .unwrap();
    assert_eq!(text(&ws, "a.txt"), "three\n");
    assert_no_temporaries(dir.path());
}

// --------------------------------------- observed versus patch-authorized mutation

#[test]
fn an_unobserved_target_is_refused_observed_and_allowed_for_patch_authorized() {
    let (dir, ws) = workspace(&[("a.txt", "one\n")]);
    let observed = ObservedFiles::new();

    for change in [
        Change::write("a.txt", "x\n"),
        Change::remove("a.txt"),
        Change::rename("a.txt", "b.txt"),
    ] {
        let result = ws.commit(
            std::slice::from_ref(&change),
            &observed,
            MutationPolicy::Observed,
        );
        assert!(
            matches!(&result, Err(MutationError::Io(message))
                if message == "You must read a.txt before changing it."),
            "{change:?}: {result:?}"
        );
    }
    assert_eq!(text(&ws, "a.txt"), "one\n");
    assert!(!dir.path().join("b.txt").exists());

    // Patch-authorized changes the same unobserved target, and records what it wrote.
    ws.commit(
        &[Change::write("a.txt", "patched\n")],
        &observed,
        MutationPolicy::PatchAuthorized,
    )
    .unwrap();
    assert_eq!(text(&ws, "a.txt"), "patched\n");
    assert_eq!(
        observed.check_unchanged(&ws.root().join("a.txt"), b"patched\n"),
        Observation::Unchanged
    );

    // A brand-new file needs no observation under either policy.
    ws.commit(
        &[Change::create("new.txt", "n\n")],
        &observed,
        MutationPolicy::Observed,
    )
    .unwrap();
    assert_eq!(text(&ws, "new.txt"), "n\n");
    assert_no_temporaries(dir.path());
}

// ------------------------------------------------------------------- denied roots

#[cfg(unix)]
#[test]
fn a_denied_root_is_refused() {
    use std::os::unix::fs::symlink;

    let (dir, ws) = workspace(&[("sub/a.txt", "a\n")]);
    let outside = outside_dir();
    fs::write(outside.path().join("secret.txt"), "secret\n").unwrap();
    // The workspace root's own parent, reached through a link that lives inside it.
    let parent = dir.path().parent().unwrap().to_path_buf();
    symlink(&parent, dir.path().join("up")).unwrap();

    let denied = [
        "..".to_string(),
        "../outside.txt".to_string(),
        "sub/../../etc/passwd".to_string(),
        "/etc/passwd".to_string(),
        outside
            .path()
            .join("secret.txt")
            .to_string_lossy()
            .into_owned(),
        "up".to_string(),
        "up/etc/passwd".to_string(),
    ];
    let observed = ObservedFiles::new();
    for path in &denied {
        let path = path.as_str();
        for error in [
            ws.stat(path).unwrap_err(),
            ws.list(path).unwrap_err(),
            ws.read(path, &observed).unwrap_err(),
        ] {
            assert!(
                matches!(error, WorkspaceError::OutsideWorkspace { .. }),
                "{path}: {error:?}"
            );
        }
        let result = ws.commit(
            &[Change::write(path, "pwned\n")],
            &observed,
            MutationPolicy::PatchAuthorized,
        );
        assert!(
            matches!(result, Err(MutationError::OutsideWorkspace { .. })),
            "{path}: {result:?}"
        );
    }

    // The root itself resolves inside but is a directory with no parent to act in.
    let root_change = ws.commit(
        &[Change::write("", "pwned\n")],
        &observed,
        MutationPolicy::PatchAuthorized,
    );
    assert!(
        matches!(root_change, Err(MutationError::WrongKind { .. })),
        "{root_change:?}"
    );

    assert_eq!(
        fs::read_to_string(outside.path().join("secret.txt")).unwrap(),
        "secret\n"
    );
    assert_eq!(entries(outside.path()), vec!["secret.txt".to_string()]);
    assert_eq!(text(&ws, "sub/a.txt"), "a\n");
    assert_no_temporaries(dir.path());
}

// ------------------------------------------------- absolute in-workspace paths

#[test]
fn an_absolute_in_workspace_path_is_accepted() {
    let (dir, ws) = workspace(&[("sub/a.txt", "one\n")]);
    let observed = ObservedFiles::new();
    let absolute = ws.root().join("sub/a.txt");
    let absolute = absolute.to_str().unwrap();

    assert_eq!(ws.resolve(absolute).unwrap(), ws.root().join("sub/a.txt"));
    assert_eq!(ws.check_path(absolute).unwrap().display(), "sub/a.txt");
    assert_eq!(ws.read(absolute, &observed).unwrap().read(0, 100), b"one\n");
    ws.commit(
        &[Change::write(absolute, "two\n")],
        &observed,
        MutationPolicy::Observed,
    )
    .unwrap();
    assert_eq!(text(&ws, "sub/a.txt"), "two\n");

    // A create at an absolute in-root path with missing parents stays inside.
    let deep = dir.path().join("sub/deep/new.txt");
    ws.commit(
        &[Change::create(deep.to_str().unwrap(), "n\n")],
        &observed,
        MutationPolicy::Observed,
    )
    .unwrap();
    assert_eq!(text(&ws, "sub/deep/new.txt"), "n\n");
    assert_no_temporaries(dir.path());
}

// --------------------------------------------------------------- unobserved read

#[test]
fn read_unobserved_leaves_the_path_never_observed() {
    let (_dir, ws) = workspace(&[("a.txt", "one\n")]);
    let observed = ObservedFiles::new();

    let snapshot = ws.read_unobserved("a.txt").unwrap();
    assert_eq!(snapshot.read(0, 100), b"one\n");
    // The agent's own registry is untouched: search can read and cannot gain edit
    // permission by doing so.
    assert_eq!(
        observed.check_unchanged(&ws.root().join("a.txt"), b"one\n"),
        Observation::NeverObserved
    );
    assert_eq!(
        ws.commit(
            &[Change::write("a.txt", "x\n")],
            &observed,
            MutationPolicy::Observed
        ),
        Err(MutationError::Io(
            "You must read a.txt before changing it.".to_string()
        ))
    );

    // Even repeated unobserved reads record nothing, and the snapshot matches a read.
    let _ = ws.read_unobserved("a.txt").unwrap();
    assert_eq!(
        observed.check_unchanged(&ws.root().join("a.txt"), b"one\n"),
        Observation::NeverObserved
    );
    let regular = ws.read("a.txt", &ObservedFiles::new()).unwrap();
    assert_eq!(snapshot.metadata(), regular.metadata());
}

// ------------------------------------------------------------------- atomicity

/// A refusal while staging — one unobserved file last in the list — happens before the
/// first replacement, so every target keeps its bytes and no temporary survives.
#[test]
fn a_failure_before_the_first_replacement_leaves_every_target_untouched() {
    let (dir, ws) = workspace(&[("a.txt", "a\n"), ("b.txt", "b\n"), ("never.txt", "n\n")]);
    let observed = ObservedFiles::new();
    ws.read("a.txt", &observed).unwrap();
    ws.read("b.txt", &observed).unwrap();

    let result = ws.commit(
        &[
            Change::write("a.txt", "A\n"),
            Change::create("new/c.txt", "C\n"),
            Change::write("never.txt", "N\n"),
        ],
        &observed,
        MutationPolicy::Observed,
    );

    assert!(
        matches!(&result, Err(MutationError::Io(message))
            if message == "You must read never.txt before changing it."),
        "{result:?}"
    );
    assert_eq!(text(&ws, "a.txt"), "a\n");
    assert_eq!(text(&ws, "b.txt"), "b\n");
    assert_eq!(text(&ws, "never.txt"), "n\n");
    assert!(!dir.path().join("new/c.txt").exists());
    assert_no_temporaries(dir.path());
}

/// The documented limit, asserted as documented behaviour and not as a guarantee. The
/// docs (`docs/design/modules/workspace-mutation.md`, "What atomicity means", matching
/// the crate docs) state: atomicity is per file; a component that changes several files
/// makes several changes under one held gate, and a crash or a trap between two changes
/// leaves the earlier ones applied, with no multi-file crash atomicity, exactly as the
/// native `apply_patch` has none. The component-facing form of "several changes under one
/// held gate" is a sequence of `OwnedMutation` calls; here the second fails (a stand-in
/// for the interruption), and the first is observed to stay. No case in this file tries
/// to prove multi-file crash atomicity.
#[test]
fn a_later_change_failing_leaves_the_earlier_applied_no_multi_file_atomicity() {
    let (dir, ws) = workspace(&[("a.txt", "a\n"), ("b.txt", "b\n")]);
    let observed = ObservedFiles::new();
    // Only `a.txt` is observed, so the second change is refused.
    ws.read("a.txt", &observed).unwrap();

    let owned = ready(ws.begin_owned(&observed, &ReadRecord::new(), MutationPolicy::Observed));
    owned.write("a.txt", "A\n").unwrap();
    assert_eq!(text(&ws, "a.txt"), "A\n");
    let second = owned.write("b.txt", "B\n");
    drop(owned);

    assert!(
        matches!(&second, Err(MutationError::Io(message))
            if message == "You must read b.txt before changing it."),
        "{second:?}"
    );
    // Per file: the applied change stands, the refused one changed nothing. There is no
    // rollback of the earlier file, and this is the level at which the capability stops.
    assert_eq!(text(&ws, "a.txt"), "A\n");
    assert_eq!(text(&ws, "b.txt"), "b\n");
    assert_no_temporaries(dir.path());
}

// ------------------------------------------------ through the components (U-mut.2)

/// A release directory holding the built components of `packages`, laid out as p1's release
/// archive ships them, with its own manifest: these cases need no other package installed.
struct Release {
    dir: tempfile::TempDir,
}

impl Release {
    /// The components as `scripts/build-modules.sh` published them.
    fn of(packages: &[&str]) -> Self {
        let built = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules");
        let release = Self {
            dir: tempfile::tempdir().expect("release dir"),
        };
        let mut components = Vec::new();
        for package in packages {
            let read = |path: PathBuf| {
                fs::read(&path).unwrap_or_else(|error| {
                    panic!(
                        "the {package} artifact {} is missing ({error}): run scripts/build-modules.sh first",
                        path.display()
                    )
                })
            };
            let wasm = read(built.join(package).join(format!("{package}.wasm")));
            let manifest: Value = serde_json::from_slice(&read(
                built.join(package).join(format!("{package}.manifest.json")),
            ))
            .expect("the package manifest is JSON");
            let path = format!("packages/{package}/{package}.wasm");
            let component = release.dir.path().join(&path);
            fs::create_dir_all(component.parent().unwrap()).expect("package dir");
            fs::write(&component, &wasm).expect("component file");
            components.push(json!({
                "name": manifest["name"],
                "digest": manifest["digest"],
                "path": path,
                "kind": manifest["kind"],
                "world": manifest["world"],
                "protocol": manifest["protocol"],
                "capabilities": manifest["capabilities"],
                "variant": manifest["variant"],
            }));
        }
        let listing = json!({ "format": "p1-release-manifest/1", "components": components });
        fs::write(release.manifest_file(), listing.to_string()).expect("manifest");
        release
    }

    fn manifest_file(&self) -> PathBuf {
        self.dir.path().join("manifest.json")
    }

    /// `name` loaded by the production loader and linked with the services the host links
    /// the module's catalog row with (`p1_host::catalog::capability_services_for`), over
    /// this agent: the row's mutation mode and one read record shared by the component's
    /// read side and its mutation.
    fn tool(&self, name: &str, ws: &Workspace, observed: &ObservedFiles) -> Arc<dyn Tool> {
        self.with_services(
            name,
            p1_host::catalog::capability_services_for(name, ws.clone(), observed.clone(), None),
        )
    }

    /// The same, linked with `services` a case assembled itself (another mutation mode than
    /// the row's, or a wrapped mutation service).
    fn with_services(&self, name: &str, services: Services) -> Arc<dyn Tool> {
        let manifest = ReleaseManifest::read(&self.manifest_file()).expect("release manifest");
        let loaded = Loader::new(manifest, self.dir.path())
            .expect("loader")
            .load(name)
            .unwrap_or_else(|error| panic!("{name} loads by name: {error}"));
        wasm_tool(
            &loaded,
            services,
            ExecutionLimits::default(),
            &Arc::new(MaskCounter::new()),
        )
        .unwrap_or_else(|error| panic!("{name} is a tool: {error}"))
    }
}

/// The services of one agent for a case that assembles another mutation mode than its
/// module's catalog row: the read side and the mutation share one read record too.
fn services_with(ws: &Workspace, observed: &ObservedFiles, policy: MutationPolicy) -> Services {
    p1_tool_read::tool_services(ws.clone(), observed.clone(), None, Some(policy))
}

fn write_call(path: &str, content: &str) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: "write".into(),
        input: ToolInput::Json(json!({ "file_path": path, "content": content }).to_string()),
    }
}

fn patch_call(tool: &dyn Tool, patch: &str) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: tool.declaration().name.clone(),
        input: ToolInput::Text(patch.to_owned()),
    }
}

async fn run(tool: &dyn Tool, call: &ToolCall) -> ToolOutcome {
    tool.execute(
        call,
        ToolContext {
            cancel: CancellationToken::new(),
        },
    )
    .await
}

/// The same call through the native `write` of an agent with its own observations.
async fn native_write(ws: &Workspace, observed: &ObservedFiles, call: &ToolCall) -> ToolOutcome {
    run(
        &p1_tool_write::WriteTool::new(ws.clone(), observed.clone()),
        call,
    )
    .await
}

// `WasmTool` starts its executor on the current Tokio runtime and the services' file work
// runs on its blocking pool: every component case runs inside a multi-threaded runtime.

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_write_component_refuses_a_never_observed_existing_file() {
    within_deadline("never observed", async {
        let (dir, ws) = workspace(&[("a.txt", "one\n")]);
        let release = Release::of(&["p1-module-write"]);
        let observed = ObservedFiles::new();
        let write = release.tool("p1/write", &ws, &observed);

        let call = write_call("a.txt", "two\n");
        let refused = run(write.as_ref(), &call).await;
        assert_eq!(refused.status, ToolStatus::Error);
        assert_eq!(refused.content, "You must read a.txt before changing it.");
        let native = native_write(&ws, &ObservedFiles::new(), &call).await;
        assert_eq!(
            (refused.status, &refused.content),
            (native.status, &native.content)
        );
        assert_eq!(text(&ws, "a.txt"), "one\n");

        // Once this agent observed the current contents, the same call writes, and what it
        // wrote is this agent's observation.
        observed.record(&ws.root().join("a.txt"), b"one\n");
        let wrote = run(write.as_ref(), &call).await;
        assert_eq!(wrote.status, ToolStatus::Ok, "{}", wrote.content);
        assert_eq!(text(&ws, "a.txt"), "two\n");
        assert_eq!(
            observed.check_unchanged(&ws.root().join("a.txt"), b"two\n"),
            Observation::Unchanged
        );
        // A new file needs no observation.
        let created = run(write.as_ref(), &write_call("sub/new.txt", "n\n")).await;
        assert_eq!(created.status, ToolStatus::Ok, "{}", created.content);
        assert_eq!(text(&ws, "sub/new.txt"), "n\n");
        assert_no_temporaries(dir.path());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_write_component_refuses_a_stale_observation() {
    within_deadline("stale", async {
        let (dir, ws) = workspace(&[("a.txt", "one\n")]);
        let release = Release::of(&["p1-module-write"]);
        let observed = ObservedFiles::new();
        observed.record(&ws.root().join("a.txt"), b"one\n");
        let write = release.tool("p1/write", &ws, &observed);

        // Another writer changed the file after this agent observed it.
        fs::write(dir.path().join("a.txt"), "changed\n").unwrap();
        let call = write_call("a.txt", "mine\n");
        let refused = run(write.as_ref(), &call).await;
        assert_eq!(refused.status, ToolStatus::Error);
        assert_eq!(
            refused.content,
            "a.txt changed on disk since you last read it; read it again."
        );
        let native_observed = ObservedFiles::new();
        native_observed.record(&ws.root().join("a.txt"), b"one\n");
        let native = native_write(&ws, &native_observed, &call).await;
        assert_eq!(
            (refused.status, &refused.content),
            (native.status, &native.content)
        );
        assert_eq!(text(&ws, "a.txt"), "changed\n");
        assert_no_temporaries(dir.path());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_escaping_path_is_outside_workspace_through_the_write_component() {
    within_deadline("escaping", async {
        let (dir, ws) = workspace(&[("inside.txt", "inside\n")]);
        let outside = outside_dir();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
        let release = Release::of(&["p1-module-write"]);
        let observed = ObservedFiles::new();
        let write = release.tool("p1/write", &ws, &observed);

        let absolute = outside.path().join("abs.txt");
        let mut escaping = vec![
            "../escape.txt".to_owned(),
            "sub/../../escape.txt".to_owned(),
            absolute.to_string_lossy().into_owned(),
        ];
        if cfg!(unix) {
            escaping.push("link/new.txt".to_owned());
        }
        for path in &escaping {
            let call = write_call(path, "pwned\n");
            let refused = run(write.as_ref(), &call).await;
            assert_eq!(refused.status, ToolStatus::Error, "{path}");
            assert!(
                refused.content.contains("escapes workspace"),
                "{path}: {}",
                refused.content
            );
            let native = native_write(&ws, &ObservedFiles::new(), &call).await;
            assert_eq!(
                (refused.status, &refused.content),
                (native.status, &native.content),
                "{path}"
            );
        }
        assert_eq!(entries(outside.path()), Vec::<String>::new());
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
        assert_no_temporaries(dir.path());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_gate_is_released_when_the_components_call_returns() {
    within_deadline("gate released", async {
        let (dir, ws) = workspace(&[]);
        let release = Release::of(&["p1-module-write"]);
        let observed = ObservedFiles::new();
        let write = release.tool("p1/write", &ws, &observed);

        for (round, content) in ["first\n", "second\n", "third\n"].into_iter().enumerate() {
            let outcome = run(write.as_ref(), &write_call("a.txt", content)).await;
            assert_eq!(
                outcome.status,
                ToolStatus::Ok,
                "round {round}: {}",
                outcome.content
            );
            assert_eq!(text(&ws, "a.txt"), content);
            // The call has returned, so its mutation is gone and the gate is free at once:
            // `ready` panics on a gate still held.
            drop(ready(ws.begin_owned(
                &ObservedFiles::new(),
                &ReadRecord::new(),
                MutationPolicy::Observed,
            )));
            // So is the native tools' synchronous side of the same gate.
            drop(ws.begin_mutation());
        }
        assert_no_temporaries(dir.path());
    })
    .await;
}

/// The runtime's mutation service, wrapped to say when a call has asked for the gate, so a
/// case orders "the call waits on the gate" before its next step without sleeping.
struct Announcing {
    inner: Arc<dyn MutationService>,
    waiting: tokio::sync::mpsc::UnboundedSender<()>,
}

impl MutationService for Announcing {
    fn begin(&self) -> BoxFuture<'_, Box<dyn HeldMutation>> {
        let _ = self.waiting.send(());
        self.inner.begin()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_call_waiting_for_a_held_gate_returns_promptly_when_cancelled() {
    within_deadline("cancelled begin", async {
        let (dir, ws) = workspace(&[]);
        let release = Release::of(&["p1-module-write"]);
        let observed = ObservedFiles::new();
        let (waiting, mut waits) = tokio::sync::mpsc::unbounded_channel();
        // The services are call-scoped (ADR-0091), so each call's mutation is the one wrapped.
        let scope = services_with(&ws, &observed, MutationPolicy::Observed)
            .call_scope
            .expect("the row's services are call-scoped");
        let services = Services::call_scoped(move || {
            let mut call = scope();
            call.workspace_mutation = Some(Arc::new(Announcing {
                inner: call
                    .workspace_mutation
                    .take()
                    .expect("the mutating row links a mutation service"),
                waiting: waiting.clone(),
            }));
            call
        });
        let write = release.with_services("p1/write", services);

        // Another agent holds the gate for as long as this case runs.
        let other = ready(ws.begin_owned(
            &ObservedFiles::new(),
            &ReadRecord::new(),
            MutationPolicy::Observed,
        ));
        let cancel = CancellationToken::new();
        let call = {
            let write = write.clone();
            let cancel = cancel.clone();
            tokio::spawn(async move {
                write
                    .execute(&write_call("a.txt", "late\n"), ToolContext { cancel })
                    .await
            })
        };
        waits.recv().await.expect("the call asks for the gate");
        cancel.cancel();
        let outcome = call.await.expect("the call task");
        assert_eq!(outcome.status, ToolStatus::Cancelled, "{}", outcome.content);
        assert!(!dir.path().join("a.txt").exists());
        drop(other);

        // Nothing the cancelled call left behind keeps the gate: the next call writes.
        let outcome = run(write.as_ref(), &write_call("a.txt", "next\n")).await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
        assert_eq!(text(&ws, "a.txt"), "next\n");
        assert_no_temporaries(dir.path());
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_patch_component_mutates_patch_authorized_and_only_when_the_host_says_so() {
    within_deadline("patch authorized", async {
        let (dir, ws) = workspace(&[("a.txt", "one\n")]);
        let outside = outside_dir();
        let release = Release::of(&["p1-module-patch"]);
        let update = "*** Begin Patch\n*** Update File: a.txt\n@@\n-one\n+ONE\n*** End Patch\n";

        // Assembled with the observed policy, the same component is refused by the host under
        // the gate: the component checks no observation itself, so the refusal is the
        // host's, and the exemption is never the module's to take.
        let observed = ObservedFiles::new();
        let strict = release.with_services(
            "p1/patch",
            services_with(&ws, &observed, MutationPolicy::Observed),
        );
        let refused = run(strict.as_ref(), &patch_call(strict.as_ref(), update)).await;
        assert_eq!(refused.status, ToolStatus::Error, "{}", refused.content);
        assert!(
            refused
                .content
                .contains("You must read a.txt before changing it."),
            "{}",
            refused.content
        );
        assert_eq!(text(&ws, "a.txt"), "one\n");

        // Patch-authorized: the never-observed file is changed, and the host records what it
        // wrote as this agent's observation.
        let observed = ObservedFiles::new();
        let patch = release.tool("p1/patch", &ws, &observed);
        let applied = run(patch.as_ref(), &patch_call(patch.as_ref(), update)).await;
        assert_eq!(applied.status, ToolStatus::Ok, "{}", applied.content);
        assert_eq!(text(&ws, "a.txt"), "ONE\n");
        assert_eq!(
            observed.check_unchanged(&ws.root().join("a.txt"), b"ONE\n"),
            Observation::Unchanged
        );

        // The exemption is from read-before-mutate, never from confinement.
        let escaping = "*** Begin Patch\n*** Add File: ../escape.txt\n+pwned\n*** End Patch\n";
        let refused = run(patch.as_ref(), &patch_call(patch.as_ref(), escaping)).await;
        assert_eq!(refused.status, ToolStatus::Error, "{}", refused.content);
        assert!(!dir.path().parent().unwrap().join("escape.txt").exists());
        assert_eq!(entries(outside.path()), Vec::<String>::new());
        drop(ready(ws.begin_owned(
            &ObservedFiles::new(),
            &ReadRecord::new(),
            MutationPolicy::Observed,
        )));
        assert_no_temporaries(dir.path());
    })
    .await;
}
