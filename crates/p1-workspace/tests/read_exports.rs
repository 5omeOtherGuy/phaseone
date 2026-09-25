//! Read side of the `workspace` and `snapshot` capability interfaces
//! (migration slice S1.2).
//!
//! `check_path`, `stat`, `list` and `read`-into-a-`Snapshot` are checked here
//! against a scratch workspace, together with the observation `read` records and
//! the `snapshot` interface's own `metadata`/`read`.

use std::fs;
use std::path::Path;

use p1_workspace::{
    FileKind, Observation, ObservedFiles, Snapshot, StreamingHash, Workspace, WorkspaceError,
};

fn workspace(root: &Path) -> Workspace {
    Workspace::new(root).unwrap()
}

#[test]
fn check_path_accepts_inside_paths_and_refuses_escapes() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/a.rs"), b"fn main() {}\n").unwrap();
    fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
    let workspace = workspace(dir.path());

    let checked = workspace.check_path("src/a.rs").unwrap();
    assert_eq!(checked.path(), workspace.root().join("src/a.rs"));
    assert_eq!(checked.display(), "src/a.rs");

    // The same file reached by its absolute path is accepted and displays alike.
    let absolute = workspace.root().join("src/a.rs");
    let checked = workspace.check_path(absolute.to_str().unwrap()).unwrap();
    assert_eq!(checked.path(), absolute);
    assert_eq!(checked.display(), "src/a.rs");

    // A `..` escape and an absolute path outside the workspace are refused.
    let escaped = workspace.check_path("../secret.txt").unwrap_err();
    assert!(
        matches!(escaped, WorkspaceError::OutsideWorkspace { .. }),
        "{escaped:?}"
    );
    let escaped = workspace
        .check_path(outside.path().join("secret.txt").to_str().unwrap())
        .unwrap_err();
    assert!(
        matches!(escaped, WorkspaceError::OutsideWorkspace { .. }),
        "{escaped:?}"
    );
}

#[cfg(unix)]
#[test]
fn check_path_refuses_a_symlink_escape() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("link")).unwrap();
    let workspace = workspace(dir.path());

    let escaped = workspace.check_path("link/secret.txt").unwrap_err();

    assert!(
        matches!(escaped, WorkspaceError::OutsideWorkspace { .. }),
        "{escaped:?}"
    );
}

#[test]
fn stat_reports_a_file_a_directory_and_a_missing_path() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("subdir")).unwrap();
    fs::write(dir.path().join("a.txt"), b"alpha\n").unwrap();
    let workspace = workspace(dir.path());

    let file = workspace.stat("a.txt").unwrap();
    assert_eq!(file.kind, FileKind::File);
    assert_eq!(file.size, 6);

    let directory = workspace.stat("subdir").unwrap();
    assert_eq!(directory.kind, FileKind::Directory);

    let missing = workspace.stat("nope.txt").unwrap_err();
    assert!(
        matches!(missing, WorkspaceError::NotFound { .. }),
        "{missing:?}"
    );
    assert!(missing.to_string().contains("nope.txt"), "{missing}");
}

#[test]
fn list_is_sorted_and_reports_kinds() {
    let dir = tempfile::tempdir().unwrap();
    let inner = dir.path().join("sub");
    fs::create_dir_all(&inner).unwrap();
    fs::write(inner.join("b.txt"), b"b").unwrap();
    fs::write(inner.join("zz.txt"), b"z").unwrap();
    fs::create_dir(inner.join("dir")).unwrap();
    fs::write(inner.join("a.txt"), b"a").unwrap();
    let workspace = workspace(dir.path());

    let entries = workspace.list("sub").unwrap();

    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["a.txt", "b.txt", "dir", "zz.txt"],
        "{entries:?}"
    );
    assert_eq!(entries[0].kind, FileKind::File);
    assert_eq!(entries[1].kind, FileKind::File);
    assert_eq!(entries[2].kind, FileKind::Directory);

    // The workspace root lists too, and is reported as a directory by stat.
    assert_eq!(workspace.list("").unwrap().len(), 1);
    assert_eq!(workspace.stat("").unwrap().kind, FileKind::Directory);

    // A file cannot be listed; a missing path is the typed absence.
    let not_a_directory = workspace.list("sub/a.txt").unwrap_err();
    assert!(
        matches!(not_a_directory, WorkspaceError::NotADirectory(_)),
        "{not_a_directory:?}"
    );
    let missing = workspace.list("sub/nope").unwrap_err();
    assert!(
        matches!(missing, WorkspaceError::NotFound { .. }),
        "{missing:?}"
    );
}

#[cfg(unix)]
#[test]
fn list_reports_a_symlink_without_following_it() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
    fs::write(dir.path().join("a.txt"), b"a").unwrap();
    std::os::unix::fs::symlink(dir.path().join("a.txt"), dir.path().join("inside-link")).unwrap();
    std::os::unix::fs::symlink(outside.path(), dir.path().join("out-link")).unwrap();
    std::os::unix::fs::symlink(dir.path().join("gone"), dir.path().join("dangling")).unwrap();
    let workspace = workspace(dir.path());

    let entries = workspace.list("").unwrap();

    let names: Vec<&str> = entries.iter().map(|entry| entry.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["a.txt", "dangling", "inside-link", "out-link"],
        "{entries:?}"
    );
    let kind = |name: &str| {
        entries
            .iter()
            .find(|entry| entry.name == name)
            .unwrap()
            .kind
    };
    // Neither a link out of the workspace nor one inside it is followed while
    // listing: the entry is the link itself.
    assert_eq!(kind("out-link"), FileKind::Symlink);
    assert_eq!(kind("inside-link"), FileKind::Symlink);
    // A link whose target exists and is inside resolves to that target.
    assert_eq!(workspace.stat("inside-link").unwrap().kind, FileKind::File);
    // A link whose target is gone has nothing to resolve to, so stat reports
    // the link itself rather than claiming the target is absent.
    assert_eq!(workspace.stat("dangling").unwrap().kind, FileKind::Symlink);
}

#[test]
fn read_returns_a_snapshot_and_records_the_observation() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    let contents = b"alpha\nbeta\ngamma\n".to_vec();
    fs::write(&path, &contents).unwrap();
    let workspace = workspace(dir.path());
    let observed = ObservedFiles::new();

    let snapshot = workspace.read("a.txt", &observed).unwrap();

    assert_eq!(snapshot.read(0, contents.len()), contents.as_slice());
    assert_eq!(
        observed.check_unchanged(&path, &contents),
        Observation::Unchanged
    );

    // The file changes on disk: the recorded observation now reports it, exactly
    // as it would after a `read` tool call.
    let changed = b"alpha\nBETA\ngamma\n".to_vec();
    fs::write(&path, &changed).unwrap();
    assert_eq!(
        observed.check_unchanged(&path, &changed),
        Observation::ChangedSinceObserved
    );

    // A second read re-observes the new contents.
    let snapshot = workspace.read("a.txt", &observed).unwrap();
    assert_eq!(snapshot.read(0, 100), changed.as_slice());
    assert_eq!(
        observed.check_unchanged(&path, &changed),
        Observation::Unchanged
    );
}

#[test]
fn read_of_an_empty_file_observes_the_empty_contents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("empty.txt");
    fs::write(&path, b"").unwrap();
    let workspace = workspace(dir.path());
    let observed = ObservedFiles::new();

    let snapshot = workspace.read("empty.txt", &observed).unwrap();

    assert_eq!(snapshot.metadata().size, 0);
    assert_eq!(snapshot.read(0, 10), &[] as &[u8]);
    assert_eq!(observed.check_unchanged(&path, b""), Observation::Unchanged);
}

#[test]
fn read_records_the_hash_a_streamed_read_of_the_same_bytes_records() {
    // Not a UTF-8 text file on purpose: `Workspace::read` is the byte-level read
    // under the reading tool, whose UTF-8 and binary rules stay in the tool.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bytes.bin");
    let contents: Vec<u8> = (0..8192u32).map(|index| (index % 251) as u8).collect();
    fs::write(&path, &contents).unwrap();
    let workspace = workspace(dir.path());
    let via_read = ObservedFiles::new();
    let via_stream = ObservedFiles::new();

    workspace.read("bytes.bin", &via_read).unwrap();
    // The same bytes as the `read` tool feeds them: chunk by chunk, through the
    // streaming hash. Its verdict on every later state must agree with ours.
    let mut hash = StreamingHash::new();
    for chunk in contents.chunks(7) {
        hash.update(chunk);
    }
    via_stream.record_streamed(&path, hash);

    for candidate in [contents.as_slice(), b"", b"other bytes"] {
        assert_eq!(
            via_read.check_unchanged(&path, candidate),
            via_stream.check_unchanged(&path, candidate),
            "candidate {candidate:?}"
        );
    }
    assert_eq!(
        via_read.check_unchanged(&path, &contents),
        Observation::Unchanged
    );
}

#[test]
fn snapshot_metadata_and_read_round_trip() {
    let dir = tempfile::tempdir().unwrap();
    let contents = b"alpha\nbeta\ngamma\n".to_vec();
    fs::write(dir.path().join("a.txt"), &contents).unwrap();
    let workspace = workspace(dir.path());
    let observed = ObservedFiles::new();

    let snapshot = workspace.read("a.txt", &observed).unwrap();

    let metadata = snapshot.metadata();
    assert_eq!(metadata.path, "a.txt");
    assert_eq!(metadata.size, contents.len() as u64);
    // The whole range returns exactly the bytes read ...
    assert_eq!(
        snapshot.read(0, metadata.size as usize),
        contents.as_slice()
    );
    // ... a sub-range returns that range, on byte offsets, not lines ...
    assert_eq!(snapshot.read(6, 4), b"beta");
    assert_eq!(snapshot.read(0, 0), b"");
    // ... and a range that runs past the end returns what exists, never panics.
    assert_eq!(snapshot.read(0, 10_000), contents.as_slice());
    assert_eq!(
        snapshot.read(contents.len() - 3, 10_000),
        &contents[contents.len() - 3..]
    );
    assert_eq!(snapshot.read(contents.len(), 10), b"");
    assert_eq!(snapshot.read(contents.len() + 5, 3), b"");
    assert_eq!(snapshot.read(usize::MAX, usize::MAX), b"");
    assert_eq!(snapshot.read(usize::MAX - 1, usize::MAX), b"");

    // Debug shows the metadata, never the contents of a possibly huge file.
    let debug = format!("{snapshot:?}");
    assert!(debug.contains("a.txt"), "{debug}");
    assert!(!debug.contains("beta"), "{debug}");
}

#[test]
fn snapshot_metadata_hash_follows_the_contents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    fs::write(&path, b"one\n").unwrap();
    let workspace = workspace(dir.path());
    let observed = ObservedFiles::new();

    let first = workspace.read("a.txt", &observed).unwrap().metadata();
    let again = workspace.read("a.txt", &observed).unwrap().metadata();
    assert_eq!(first.content_hash, again.content_hash);

    fs::write(&path, b"two\n").unwrap();
    let changed = workspace.read("a.txt", &observed).unwrap().metadata();
    assert_ne!(first.content_hash, changed.content_hash);
    assert_eq!(first.size, changed.size, "same length, different hash");
}

#[test]
fn a_snapshot_is_unaffected_by_a_later_change_of_the_file() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("a.txt");
    fs::write(&path, b"before").unwrap();
    let workspace = workspace(dir.path());
    let observed = ObservedFiles::new();
    let snapshot = workspace.read("a.txt", &observed).unwrap();
    let metadata = snapshot.metadata();

    fs::write(&path, b"after!").unwrap();

    assert_eq!(snapshot.read(0, 100), b"before");
    assert_eq!(snapshot.metadata(), metadata);
    // A fresh read sees the new state; the old snapshot keeps its own.
    assert_eq!(
        workspace.read("a.txt", &observed).unwrap().read(0, 100),
        b"after!"
    );
}

#[test]
fn read_refuses_paths_outside_the_workspace_and_a_missing_path() {
    let dir = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let secret = outside.path().join("secret.txt");
    fs::write(&secret, b"secret").unwrap();
    let workspace = workspace(dir.path());
    let observed = ObservedFiles::new();

    let escaped = workspace.read("../secret.txt", &observed).unwrap_err();
    assert!(
        matches!(escaped, WorkspaceError::OutsideWorkspace { .. }),
        "{escaped:?}"
    );
    let escaped = workspace
        .read(secret.to_str().unwrap(), &observed)
        .unwrap_err();
    assert!(
        matches!(escaped, WorkspaceError::OutsideWorkspace { .. }),
        "{escaped:?}"
    );
    // A refused read records nothing, so a later edit is not treated as
    // read-before-mutate.
    assert_eq!(
        observed.check_unchanged(&secret, b"secret"),
        Observation::NeverObserved
    );

    let missing_path = dir.path().join("nope.txt");
    let missing = workspace.read("nope.txt", &observed).unwrap_err();
    assert!(
        matches!(missing, WorkspaceError::NotFound { .. }),
        "{missing:?}"
    );
    assert_eq!(
        observed.check_unchanged(&missing_path, b""),
        Observation::NeverObserved
    );
}

#[test]
fn read_of_a_directory_is_the_typed_not_a_directory_error() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir(dir.path().join("subdir")).unwrap();
    let workspace = workspace(dir.path());
    let observed = ObservedFiles::new();

    // A directory is not untyped I/O: the host import maps the wrong kind without
    // parsing the io error, exactly as `list` of a file already does.
    let not_a_directory = workspace.read("subdir", &observed).unwrap_err();
    assert!(
        matches!(not_a_directory, WorkspaceError::NotADirectory(_)),
        "{not_a_directory:?}"
    );
    // The refused read recorded nothing.
    let subdir = dir.path().join("subdir");
    assert_eq!(
        observed.check_unchanged(&subdir, b""),
        Observation::NeverObserved
    );

    // The root reads as a directory too.
    let root = workspace.read("", &observed).unwrap_err();
    assert!(matches!(root, WorkspaceError::NotADirectory(_)), "{root:?}");
}

#[test]
fn a_snapshot_is_clone_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<Snapshot>();

    let dir = tempfile::tempdir().unwrap();
    fs::write(dir.path().join("a.txt"), b"bytes").unwrap();
    let workspace = workspace(dir.path());
    let observed = ObservedFiles::new();
    let snapshot = workspace.read("a.txt", &observed).unwrap();

    let clone = snapshot.clone();
    drop(snapshot);

    assert_eq!(clone.read(0, 100), b"bytes");
}
