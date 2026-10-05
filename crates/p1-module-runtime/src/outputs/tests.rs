//! The store and the service behind `tool-outputs` (ADR-0109, #510 definition of done 2-5).

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use p1_contracts::CancellationToken;
use p1_redact::SecretSet;

use super::*;
use crate::process::{ProcessCapability, ProcessService as NativeProcesses};
use crate::{ProcessCommand, ProcessEvent, ProcessService};

/// A fake key of the `sk-` shape, built at run time: no credential-shaped literal.
fn fake_key() -> String {
    format!("sk-{}", "Qw3rTy7uIo".repeat(3))
}

fn caps(per_output: u64, per_session: u64) -> OutputCaps {
    OutputCaps {
        per_output,
        per_session,
    }
}

/// A session store in a scratch directory, as `--session FILE` gives it.
fn session_store(scratch: &tempfile::TempDir, caps: OutputCaps) -> Arc<OutputStore> {
    Arc::new(OutputStore::in_directory(
        scratch.path().join("session.jsonl.outputs"),
        caps,
    ))
}

/// Ends a run's store. A writer thread keeps its own reference to the store until the thread
/// exits, which can be after `produced` returned, so the run ends when the test drops the last
/// reference: wait for that instead of racing the writer.
fn end_run(store: Arc<OutputStore>) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while Arc::strong_count(&store) > 1 {
        assert!(
            std::time::Instant::now() < deadline,
            "a writer kept the store"
        );
        std::thread::yield_now();
    }
    drop(store);
}

/// Stores `chunks` as one output of a fresh call and returns what `produced` says of it.
fn store_output(store: &Arc<OutputStore>, secrets: &SecretSet, chunks: &[&[u8]]) -> OutputInfo {
    let call = CallOutputs::new(store.clone(), secrets.clone());
    let mut recorder = call.record();
    for chunk in chunks {
        recorder.write(chunk);
    }
    recorder.finish();
    drop(recorder);
    let mut produced = call.produced();
    assert_eq!(produced.len(), 1);
    produced.remove(0)
}

/// Every page from offset zero with `limit`, concatenated, with the page count.
fn read_all(store: &OutputStore, handle: &str, limit: u32) -> (String, usize) {
    let mut text = String::new();
    let mut offset = 0;
    let mut pages = 0;
    loop {
        let page = store.page(handle, offset, limit).expect("page");
        assert_eq!(page.next_offset, offset + page.text.len() as u64);
        text.push_str(&page.text);
        offset = page.next_offset;
        pages += 1;
        if page.at_end {
            return (text, pages);
        }
    }
}

fn stored_file(store: &OutputStore, handle: &str) -> Vec<u8> {
    std::fs::read(store.directory().join(handle)).expect("the stored file")
}

// --- redaction before persistence (DoD 2) -------------------------------------------------------

#[test]
fn a_key_split_across_two_chunks_is_masked_on_disk_and_in_every_page() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let key = fake_key();
    let text = format!("export KEY={key}\nnext line\n");
    let split = text.find("Qw3rTy").unwrap() + 4;
    let info = store_output(
        &store,
        &SecretSet::new(),
        &[&text.as_bytes()[..split], &text.as_bytes()[split..]],
    );
    assert_eq!(info.capture, Capture::Complete);

    let raw = &key["sk-".len()..][..12];
    let on_disk = String::from_utf8(stored_file(&store, &info.handle)).unwrap();
    assert!(!on_disk.contains(raw), "{on_disk}");
    assert!(on_disk.contains("<redacted:sk-:"), "{on_disk}");
    assert_eq!(info.stored_bytes, on_disk.len() as u64);
    for limit in [1, 2, 7, 64, 4096] {
        let (paged, _) = read_all(&store, &info.handle, limit);
        assert_eq!(paged, on_disk, "limit {limit}");
        // No page holds a piece of the key either.
        let mut offset = 0;
        loop {
            let page = store.page(&info.handle, offset, limit).unwrap();
            assert!(!page.text.contains(raw));
            offset = page.next_offset;
            if page.at_end {
                break;
            }
        }
    }
}

#[test]
fn a_registered_credential_is_masked_before_it_is_stored() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let secrets = SecretSet::new();
    let value = "no-shape-at-all-but-registered-0042";
    secrets.register(value);
    let info = store_output(
        &store,
        &secrets,
        &[b"a no-shape-at-", b"all-but-registered-0042 b\n"],
    );
    let (paged, _) = read_all(&store, &info.handle, 4096);
    assert!(!paged.contains("no-shape-at-"), "{paged}");
    assert!(paged.contains("<redacted:secret:"), "{paged}");
}

// --- handles (DoD 3) -----------------------------------------------------------------------------

#[test]
fn a_handle_of_another_session_a_malformed_one_and_a_removed_one_are_unknown() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let ours = session_store(&first, OutputCaps::DEFAULT);
    let theirs = session_store(&second, OutputCaps::DEFAULT);
    let info = store_output(&theirs, &SecretSet::new(), &[b"theirs\n"]);
    assert!(theirs.describe(&info.handle).is_ok());

    // Another session's handle names nothing here.
    assert_eq!(ours.describe(&info.handle), Err(OutputError::UnknownOutput));
    assert_eq!(
        ours.page(&info.handle, 0, 10),
        Err(OutputError::UnknownOutput)
    );

    // Nothing but the handle's own shape is looked up: paths, the file name, other shapes.
    let file = theirs.directory().join(&info.handle);
    for malformed in [
        String::new(),
        "out-".to_owned(),
        "../session.jsonl".to_owned(),
        format!("{}.out", info.handle),
        format!("../{}", info.handle),
        file.display().to_string(),
        info.handle.to_uppercase(),
        format!("{}0", info.handle),
        info.handle.replacen("out-", "out/", 1),
    ] {
        assert_eq!(
            theirs.describe(&malformed),
            Err(OutputError::UnknownOutput),
            "{malformed}"
        );
        assert_eq!(
            theirs.page(&malformed, 0, 10),
            Err(OutputError::UnknownOutput),
            "{malformed}"
        );
    }

    // A removed output is unknown too.
    std::fs::remove_file(&file).unwrap();
    assert_eq!(
        theirs.describe(&info.handle),
        Err(OutputError::UnknownOutput)
    );
}

#[test]
fn a_handle_is_random_and_never_a_path() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let one = store_output(&store, &SecretSet::new(), &[b"1\n"]);
    let two = store_output(&store, &SecretSet::new(), &[b"2\n"]);
    assert_ne!(one.handle, two.handle);
    for handle in [&one.handle, &two.handle] {
        assert!(handle.starts_with("out-") && handle.len() == 36, "{handle}");
        assert!(!handle.contains(['/', '.', '\\']), "{handle}");
    }
}

/// Review finding 3: only what this store recorded is served, so an earlier run's outputs
/// are not served after `--resume`. A run killed before it ended leaves its directory, which
/// counts against the session cap.
#[test]
fn a_resumed_session_counts_a_killed_runs_outputs_and_serves_none_of_them() {
    let scratch = tempfile::tempdir().unwrap();
    let first = session_store(&scratch, caps(1024, 10));
    let info = store_output(&first, &SecretSet::new(), &[b"12345678\n"]);
    let earlier = first.directory().to_path_buf();
    // Killed: the run never ends, so nothing removes its directory.
    std::mem::forget(first);
    assert!(earlier.join(&info.handle).is_file());
    // `--resume`: a new store over the same session.
    let resumed = session_store(&scratch, caps(1024, 10));
    assert_ne!(resumed.directory(), earlier);
    assert_eq!(
        resumed.describe(&info.handle),
        Err(OutputError::UnknownOutput)
    );
    // The earlier nine bytes count against the session cap.
    let next = store_output(&resumed, &SecretSet::new(), &[b"abcdef\n"]);
    assert_eq!(next.capture, Capture::StoredCapReached);
    assert_eq!(next.stored_bytes, 1);
    // Its own end removes only its own directory; the killed run's keeps `FILE.outputs/`.
    let root = scratch.path().join("session.jsonl.outputs");
    let own = resumed.directory().to_path_buf();
    end_run(resumed);
    assert!(!own.exists());
    assert!(earlier.join(&info.handle).is_file());
    assert!(root.is_dir());
}

/// #523: a session run's directory is removed when the run ends, since no later run serves
/// it; `FILE.outputs/` goes with it when nothing else is left.
#[test]
fn a_session_runs_directory_is_removed_when_the_run_ends() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let info = store_output(&store, &SecretSet::new(), &[b"stored\n"]);
    let directory = store.directory().to_path_buf();
    assert!(directory.join(&info.handle).is_file());
    store.remove_run_directory();
    assert!(!directory.exists());
    assert!(!scratch.path().join("session.jsonl.outputs").exists());
    assert_eq!(
        store.describe(&info.handle),
        Err(OutputError::UnknownOutput)
    );
    let later = store_output(&store, &SecretSet::new(), &[b"later\n"]);
    assert_eq!(later.capture, Capture::StorageFailed);
    assert!(!directory.exists());
}

/// #523 review: cleanup must not follow a replaced session root into unrelated outputs.
#[test]
fn cleanup_preserves_a_same_named_run_behind_a_symlinked_session_root() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let info = store_output(&store, &SecretSet::new(), &[b"stored\n"]);
    let directory = store.directory().to_path_buf();
    let root = directory.parent().unwrap();
    let moved = scratch.path().join("original-outputs");
    std::fs::rename(root, &moved).unwrap();
    let unrelated = scratch.path().join("unrelated");
    let other_run = unrelated.join(directory.file_name().unwrap());
    std::fs::create_dir_all(&other_run).unwrap();
    let other_file = other_run.join("keep");
    std::fs::write(&other_file, b"unrelated\n").unwrap();
    std::os::unix::fs::symlink(&unrelated, root).unwrap();

    end_run(store);

    assert_eq!(std::fs::read(&other_file).unwrap(), b"unrelated\n");
    assert!(root.is_symlink());
    assert!(
        moved
            .join(directory.file_name().unwrap())
            .join(info.handle)
            .is_file()
    );
}

/// #523 review: the same path does not make a replacement directory this store's own.
#[test]
fn cleanup_preserves_a_replacement_run_directory() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let info = store_output(&store, &SecretSet::new(), &[b"stored\n"]);
    let directory = store.directory().to_path_buf();
    let moved = scratch.path().join("original-run");
    std::fs::rename(&directory, &moved).unwrap();
    std::fs::create_dir(&directory).unwrap();
    let other_file = directory.join("keep");
    std::fs::write(&other_file, b"replacement\n").unwrap();

    end_run(store);

    assert_eq!(std::fs::read(&other_file).unwrap(), b"replacement\n");
    assert!(moved.join(info.handle).is_file());
    assert!(directory.parent().unwrap().is_dir());
}

/// Cleanup is flat: non-regular entries keep the run and session directories nonempty.
#[test]
fn cleanup_leaves_non_regular_entries_in_its_run_directory() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let info = store_output(&store, &SecretSet::new(), &[b"stored\n"]);
    let directory = store.directory().to_path_buf();
    let nested = directory.join("nested");
    std::fs::create_dir(&nested).unwrap();
    std::fs::write(nested.join("keep"), b"nested\n").unwrap();
    let target = scratch.path().join("keep");
    std::fs::write(&target, b"target\n").unwrap();
    let link = directory.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();

    end_run(store);

    assert!(!directory.join(info.handle).exists());
    assert_eq!(std::fs::read(nested.join("keep")).unwrap(), b"nested\n");
    assert_eq!(std::fs::read(&target).unwrap(), b"target\n");
    assert!(link.is_symlink());
    assert!(directory.parent().unwrap().is_dir());
}

/// #523: a run that ended leaves nothing to count, so a resumed session has its whole cap.
#[test]
fn a_resumed_session_after_a_run_that_ended_has_its_whole_cap() {
    let scratch = tempfile::tempdir().unwrap();
    let first = session_store(&scratch, caps(1024, 10));
    store_output(&first, &SecretSet::new(), &[b"12345678\n"]);
    end_run(first);
    let resumed = session_store(&scratch, caps(1024, 10));
    let next = store_output(&resumed, &SecretSet::new(), &[b"abcdef\n"]);
    assert_eq!(next.capture, Capture::Complete);
    assert_eq!(next.stored_bytes, 7);
}

/// Review finding 3: a file with a handle's name that the store did not write, or an output
/// replaced or rewritten after the store recorded it, is never served.
#[test]
fn a_file_the_store_did_not_record_is_never_served() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let info = store_output(&store, &SecretSet::new(), &[b"recorded\n"]);
    let planted = "out-00000000000000000000000000000000";
    let root = scratch.path().join("session.jsonl.outputs");
    for dir in [root.as_path(), store.directory()] {
        std::fs::write(dir.join(planted), "planted unmasked text\n").unwrap();
        std::fs::write(dir.join(format!("{planted}.out")), "planted\n").unwrap();
    }
    assert_eq!(store.describe(planted), Err(OutputError::UnknownOutput));
    assert_eq!(
        store.page(planted, 0, 4096),
        Err(OutputError::UnknownOutput)
    );
    assert_eq!(read_all(&store, &info.handle, 4096).0, "recorded\n");

    // Put in its place: another inode.
    let path = store.directory().join(&info.handle);
    let other = store.directory().join("other");
    std::fs::write(&other, "replaced\n").unwrap();
    std::fs::rename(&other, &path).unwrap();
    assert_eq!(
        store.describe(&info.handle),
        Err(OutputError::UnknownOutput)
    );

    // Rewritten in place: another size.
    let info = store_output(&store, &SecretSet::new(), &[b"recorded\n"]);
    let path = store.directory().join(&info.handle);
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap()
        .write_all(b"appended\n")
        .unwrap();
    assert_eq!(
        store.page(&info.handle, 0, 4096),
        Err(OutputError::UnknownOutput)
    );
}

#[test]
fn a_same_size_in_place_rewrite_is_never_served() {
    use std::os::unix::fs::MetadataExt;

    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let info = store_output(&store, &SecretSet::new(), &[b"recorded\n"]);
    let path = store.directory().join(&info.handle);
    let before = std::fs::metadata(&path).unwrap();
    let mut file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.write_all(b"tampered\n").unwrap();
    file.sync_all().unwrap();
    let after = file.metadata().unwrap();
    assert_eq!(
        (before.dev(), before.ino(), before.len()),
        (after.dev(), after.ino(), after.len())
    );
    assert_ne!(
        (before.ctime(), before.ctime_nsec()),
        (after.ctime(), after.ctime_nsec()),
        "fixture must change only the recorded change stamp"
    );
    assert_eq!(std::fs::read(&path).unwrap(), b"tampered\n");
    assert_eq!(
        store.describe(&info.handle),
        Err(OutputError::UnknownOutput)
    );
    assert_eq!(
        store.page(&info.handle, 0, 4096),
        Err(OutputError::UnknownOutput)
    );
}

// --- the disk never slows the command (review finding 5) ------------------------------------

/// A stalled disk: the recorder never waits, stops storing when the queue is full, and the
/// output is `storage-incomplete` with exactly what was queued stored, never more than the
/// queue's byte bound.
#[test]
fn a_stalled_disk_never_blocks_the_stream_and_the_output_is_incomplete() {
    use crate::outputs::store::QUEUE_BYTES;
    // Twice the bound: every line is at least seven bytes.
    const LINES: usize = 2 * QUEUE_BYTES / "line 0\n".len();
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    store.stall.set(false);
    let call = CallOutputs::new(store.clone(), SecretSet::new());
    let (done, finished) = std::sync::mpsc::channel();
    let recording = call.clone();
    std::thread::spawn(move || {
        let mut recorder = recording.record();
        for line in 0..LINES {
            recorder.write(format!("line {line}\n").as_bytes());
        }
        recorder.finish();
        done.send(()).unwrap();
    });
    // A bound, not a timing assertion: a recorder that waited for the disk never returns.
    finished
        .recv_timeout(std::time::Duration::from_secs(60))
        .expect("the recorder waited for a stalled disk");
    store.stall.set(true);
    let info = call.produced().remove(0);
    assert_eq!(info.capture, Capture::StorageIncomplete);
    let stored = String::from_utf8(stored_file(&store, &info.handle)).unwrap();
    assert_eq!(info.stored_bytes, stored.len() as u64);
    let expected: String = (0..LINES).map(|line| format!("line {line}\n")).collect();
    assert!(!stored.is_empty() && stored.len() <= QUEUE_BYTES);
    assert!(
        expected.starts_with(&stored),
        "what is stored is exact up to where it stopped"
    );
    assert_eq!(
        store.describe(&info.handle).unwrap().capture,
        Capture::StorageIncomplete
    );
}

/// #525: a shell loop of 20,000 one-line `echo`s hands the store 20,000 small chunks faster
/// than one writer takes them one by one. The queue is bounded by bytes, so the whole burst
/// fits while the disk has not taken a byte of it, and the output is complete.
#[test]
fn a_burst_of_small_chunks_is_stored_whole_while_the_disk_waits() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let lines: Vec<String> = (1..=20_000)
        .map(|n| match n {
            9_000 => format!("error: step {n} failed\n"),
            _ => format!("step {n} ok\n"),
        })
        .collect();
    store.stall.set(false);
    let call = CallOutputs::new(store.clone(), SecretSet::new());
    let mut recorder = call.record();
    for line in &lines {
        recorder.write(line.as_bytes());
    }
    store.stall.set(true);
    recorder.finish();
    drop(recorder);
    let info = call.produced().remove(0);
    assert_eq!(info.capture, Capture::Complete);
    let expected = lines.concat();
    assert_eq!(info.stored_bytes, expected.len() as u64);
    assert_eq!(read_all(&store, &info.handle, MAX_PAGE_BYTES).0, expected);
}

/// A writer still stalled when `produced` stops waiting: the output is given up as
/// `storage-failed`, and its file is removed once the writer finishes, never served.
#[test]
fn an_output_whose_writer_does_not_settle_is_given_up() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    store.stall.set(false);
    let call = CallOutputs::new(store.clone(), SecretSet::new());
    let mut recorder = call.record();
    recorder.write(b"stalled\n");
    recorder.finish();
    let info = call.produced().remove(0);
    assert_eq!(info.capture, Capture::StorageFailed);
    assert_eq!(info.stored_bytes, 0);
    store.stall.set(true);
    let path = store.directory().join(&info.handle);
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    while path.exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "the given-up file stays"
        );
        std::thread::yield_now();
    }
    assert_eq!(
        store.describe(&info.handle),
        Err(OutputError::UnknownOutput)
    );
}

/// The same through a real process: a stalled disk neither slows nor fails the command.
#[tokio::test]
async fn a_stalled_disk_neither_slows_nor_fails_a_command() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    store.stall.set(false);
    let outputs = CallOutputs::new(store.clone(), SecretSet::new());
    let capability = ProcessCapability::new(Arc::new(
        NativeProcesses::new(workspace.path()).with_env_snapshot(Vec::new()),
    ))
    .storing(outputs.clone());
    let mut process = capability
        .spawn(
            ProcessCommand {
                script: "seq 1 300000".into(),
                timeout_ms: 600_000,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut exit = None;
    while let Some(event) = process.next().await {
        if let ProcessEvent::Exited(status) = event {
            exit = Some(status);
        }
    }
    assert_eq!(exit, Some(crate::ExitStatus::Code(0)));
    store.stall.set(true);
    let produced = tokio::task::spawn_blocking(move || outputs.produced())
        .await
        .unwrap();
    assert_eq!(produced[0].capture, Capture::StorageIncomplete);
}

// --- page (DoD 3 of the interface) ----------------------------------------------------------------

#[test]
fn pages_never_split_a_character_and_concatenate_to_the_stored_bytes() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let text = "aä€𝄞b\n".repeat(50);
    let info = store_output(&store, &SecretSet::new(), &[text.as_bytes()]);
    assert_eq!(info.stored_bytes, text.len() as u64);
    for limit in 4..=9 {
        let (paged, pages) = read_all(&store, &info.handle, limit);
        assert_eq!(paged, text, "limit {limit}");
        assert!(pages > 1);
    }
    // This small output fits in one page even for an oversized request.
    let page = store.page(&info.handle, 0, u32::MAX).unwrap();
    assert!(page.at_end && page.text == text);
}

#[test]
fn an_oversized_page_request_is_capped_and_reconstructs_the_whole_output() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let cap = super::store::MAX_PAGE_BYTES as usize;
    let text = "aä€𝄞b\n".repeat(cap / 12 + 2);
    assert!(text.len() > cap);
    let info = store_output(&store, &SecretSet::new(), &[text.as_bytes()]);
    assert_eq!(info.capture, Capture::Complete);
    assert_eq!(info.stored_bytes, text.len() as u64);
    let mut offset = 0;
    let mut paged = String::new();
    loop {
        let page = store.page(&info.handle, offset, u32::MAX).unwrap();
        assert!(!page.text.is_empty() && page.text.len() <= cap);
        assert_eq!(page.next_offset, offset + page.text.len() as u64);
        if offset == 0 {
            assert!(!page.at_end, "first page must hit the cap, not the end");
            assert!(
                page.text.len() >= cap - 3,
                "cap only backs off to a UTF-8 boundary"
            );
        }
        paged.push_str(&page.text);
        offset = page.next_offset;
        if page.at_end {
            break;
        }
    }
    assert_eq!(paged, text);
    assert_eq!(offset, info.stored_bytes);
}

#[test]
fn a_limit_too_small_an_offset_inside_a_character_and_one_past_the_end_are_refused() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    // 𝄞 is four bytes, at offset 1.
    let info = store_output(&store, &SecretSet::new(), &["a𝄞b".as_bytes()]);
    let handle = &info.handle;
    assert_eq!(store.page(handle, 1, 3), Err(OutputError::LimitTooSmall));
    assert_eq!(store.page(handle, 1, 0), Err(OutputError::LimitTooSmall));
    assert_eq!(store.page(handle, 0, 3).unwrap().text, "a");
    assert_eq!(store.page(handle, 1, 4).unwrap().text, "𝄞");
    assert_eq!(
        store.page(handle, 2, 4),
        Err(OutputError::OffsetInsideCharacter)
    );
    assert_eq!(store.page(handle, 7, 4), Err(OutputError::OffsetPastEnd(6)));
    assert_eq!(
        store.page(handle, 6, 4),
        Ok(OutputPage {
            text: String::new(),
            next_offset: 6,
            at_end: true,
        })
    );
    // The last page says it is the last.
    assert_eq!(
        store.page(handle, 5, 4),
        Ok(OutputPage {
            text: "b".to_owned(),
            next_offset: 6,
            at_end: true,
        })
    );
}

// --- capture states and caps (DoD 4, ADR item 7) ---------------------------------------------------

#[test]
fn an_active_recorder_is_not_complete_until_finished_and_flushed() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let call = CallOutputs::new(store.clone(), SecretSet::new());
    let mut recorder = call.record();
    recorder.write(b"still printing\n");
    assert_eq!(call.produced()[0].capture, Capture::StorageIncomplete);
    recorder.finish();
    let info = call.produced().remove(0);
    assert_eq!(info.capture, Capture::Complete);
    assert_eq!(read_all(&store, &info.handle, 100).0, "still printing\n");
}

#[test]
fn an_interrupted_source_flushes_held_text_without_overriding_a_store_cap() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, caps(10, 1024));
    let call = CallOutputs::new(store.clone(), SecretSet::new());
    let mut recorder = call.record();
    recorder.write(b"held text");
    recorder.mark_incomplete();
    recorder.finish();
    let info = call.produced().remove(0);
    assert_eq!(info.capture, Capture::StorageIncomplete);
    assert_eq!(read_all(&store, &info.handle, 100).0, "held text");

    let mut recorder = call.record();
    recorder.write(b"longer than the cap\n");
    recorder.mark_incomplete();
    recorder.finish();
    let info = call.produced().remove(1);
    assert_eq!(info.capture, Capture::StoredCapReached);
    assert_eq!(read_all(&store, &info.handle, 100).0, "longer tha");
}

#[test]
fn an_output_past_its_cap_is_stored_exactly_up_to_the_cap() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, caps(10, 1024));
    let info = store_output(&store, &SecretSet::new(), &[b"0123456", b"789abcdef\n"]);
    assert_eq!(info.capture, Capture::StoredCapReached);
    assert_eq!(info.stored_bytes, 10);
    assert_eq!(store.describe(&info.handle), Ok(info.clone()));
    assert_eq!(read_all(&store, &info.handle, 100).0, "0123456789");
    // A cap never splits a character either.
    let info = store_output(&store, &SecretSet::new(), &["123456789€\n".as_bytes()]);
    assert_eq!(read_all(&store, &info.handle, 100).0, "123456789");
    // An output within the cap is complete.
    let info = store_output(&store, &SecretSet::new(), &[b"short\n"]);
    assert_eq!(info.capture, Capture::Complete);
}

#[test]
fn the_session_cap_bounds_all_outputs_together() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, caps(1024, 12));
    let first = store_output(&store, &SecretSet::new(), &[b"12345678\n"]);
    assert_eq!(first.capture, Capture::Complete);
    let second = store_output(&store, &SecretSet::new(), &[b"abcdefgh\n"]);
    assert_eq!(second.capture, Capture::StoredCapReached);
    assert_eq!(second.stored_bytes, 3);
    let third = store_output(&store, &SecretSet::new(), &[b"x\n"]);
    assert_eq!(third.capture, Capture::StoredCapReached);
    assert_eq!(third.stored_bytes, 0);
    assert_eq!(read_all(&store, &third.handle, 10).0, "");
}

#[test]
fn an_unwritable_store_is_storage_failed_and_offers_no_readable_handle() {
    let scratch = tempfile::tempdir().unwrap();
    // Something that is not a directory stands where the store would be.
    std::fs::write(scratch.path().join("session.jsonl.outputs"), "in the way").unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let info = store_output(&store, &SecretSet::new(), &[b"lost\n"]);
    assert_eq!(info.capture, Capture::StorageFailed);
    assert_eq!(info.stored_bytes, 0);
    assert_eq!(
        store.describe(&info.handle),
        Err(OutputError::UnknownOutput)
    );
    assert_eq!(
        store.page(&info.handle, 0, 10),
        Err(OutputError::UnknownOutput)
    );
}

#[test]
fn a_store_that_vanishes_mid_output_is_storage_failed() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let call = CallOutputs::new(store.clone(), SecretSet::new());
    let mut recorder = call.record();
    recorder.write(b"first\n");
    std::fs::remove_dir_all(store.directory()).unwrap();
    recorder.write(b"second\n");
    recorder.finish();
    let info = call.produced().remove(0);
    assert_eq!(info.capture, Capture::StorageFailed);
    assert_eq!(info.stored_bytes, 0);
    assert_eq!(
        store.describe(&info.handle),
        Err(OutputError::UnknownOutput)
    );
}

#[test]
fn the_store_is_private_to_its_owner() {
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let info = store_output(&store, &SecretSet::new(), &[b"x\n"]);
    let mode =
        |path: &std::path::Path| std::fs::metadata(path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode(&scratch.path().join("session.jsonl.outputs")), 0o700);
    assert_eq!(mode(store.directory()), 0o700);
    assert_eq!(mode(&store.directory().join(&info.handle)), 0o600);
}

#[test]
fn a_temporary_store_is_created_on_first_use_and_removed_with_its_run() {
    let store = Arc::new(OutputStore::temporary(OutputCaps::DEFAULT));
    let info = store_output(&store, &SecretSet::new(), &[b"x\n"]);
    assert!(store.describe(&info.handle).is_ok());
    store.remove_run_directory();
    assert_eq!(
        store.describe(&info.handle),
        Err(OutputError::UnknownOutput)
    );
    let later = store_output(&store, &SecretSet::new(), &[b"y\n"]);
    assert_eq!(later.capture, Capture::StorageFailed);
}

// --- the tee on a real process (ADR item 1) ---------------------------------------------------------

#[tokio::test]
async fn a_process_output_is_stored_whole_while_the_stream_keeps_its_head_and_tail() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let store = session_store(&scratch, OutputCaps::DEFAULT);
    let outputs = CallOutputs::new(store.clone(), SecretSet::new());
    // HOME is the empty workspace, so the login shell reads no user profile: a profile's
    // stderr arrives on its own pipe and may land anywhere in what the command prints.
    let home = vec![("HOME".into(), workspace.path().as_os_str().to_owned())];
    let capability = ProcessCapability::new(Arc::new(
        NativeProcesses::new(workspace.path()).with_env_snapshot(home),
    ))
    .storing(outputs.clone());
    let mut process = capability
        .spawn(
            ProcessCommand {
                script: "seq 1 100000; echo done".into(),
                timeout_ms: 60_000,
            },
            CancellationToken::new(),
        )
        .await
        .unwrap();
    let mut shown = Vec::new();
    while let Some(event) = process.next().await {
        match event {
            ProcessEvent::Output(bytes) => shown.extend(bytes),
            ProcessEvent::Exited(_) => break,
        }
    }
    let shown = String::from_utf8(shown).unwrap();
    assert!(!shown.contains("\n50000\n"), "the stream cut the middle");
    let produced = outputs.produced();
    assert_eq!(produced.len(), 1);
    assert_eq!(produced[0].capture, Capture::Complete);
    let (stored, _) = read_all(&store, &produced[0].handle, MAX_PAGE_BYTES);
    let mut expected: String = (1..=100_000).map(|n| format!("{n}\n")).collect();
    expected.push_str("done\n");
    // A login shell's profile may print first; everything the command printed follows.
    assert!(
        stored.ends_with(&expected),
        "{}",
        &stored[..200.min(stored.len())]
    );
    // A call without the store's tee records nothing.
    assert!(
        CallOutputs::new(store, SecretSet::new())
            .produced()
            .is_empty()
    );
}

/// Manual #549 evidence: count bytes at the command's stdout inside the shipped
/// shell component, before the host tee masks them. Repository access is read-only;
/// command output and HOME live only in scratch directories.
#[tokio::test]
#[ignore = "manual case-8 measurement; requires built p1/shell and bubblewrap"]
async fn case_8_printed_vs_stored_inside_the_shell_component() {
    use crate::process::Sandbox;
    use crate::{ExecutionLimits, Loader, ReleaseManifest, Services, wasm_tool};
    use p1_contracts::serde_json::{Value, json};
    use p1_contracts::{ToolCall, ToolContext, ToolInput};

    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .unwrap();
    let built = root.join("modules/target/p1-modules");
    let manifest: Value = p1_contracts::serde_json::from_str(
        &std::fs::read_to_string(built.join("p1-module-shell/p1-module-shell.manifest.json"))
            .unwrap(),
    )
    .unwrap();
    let release = json!({"format": "p1-release-manifest/1", "components": [{
        "name": manifest["name"], "digest": manifest["digest"],
        "path": "p1-module-shell/p1-module-shell.wasm", "kind": manifest["kind"],
        "world": manifest["world"], "protocol": manifest["protocol"],
        "capabilities": manifest["capabilities"], "variant": manifest["variant"],
    }]});
    let loader = Loader::new(ReleaseManifest::parse(&release.to_string()).unwrap(), built).unwrap();
    let module = loader.load("p1/shell").unwrap();
    let common = std::process::Command::new("git")
        .current_dir(&root)
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .output()
        .unwrap();
    assert!(common.status.success());
    let common = std::path::PathBuf::from(String::from_utf8(common.stdout).unwrap().trim());
    let baseline = std::process::Command::new("bash")
        .current_dir(&root)
        .args(["-c", "git log -p -n 200"])
        .output()
        .unwrap();
    assert!(baseline.status.success());
    println!(
        "case8 bash-c printed={} stderr={}",
        baseline.stdout.len(),
        baseline.stderr.len()
    );
    drop(baseline);

    for sandboxed in [false, true] {
        // Outside /tmp so bubblewrap's private /tmp does not hide the counting file.
        let scratch = tempfile::tempdir().unwrap();
        let mut native = NativeProcesses::new(&root).with_env_snapshot(vec![
            ("PATH".into(), "/usr/bin:/bin".into()),
            ("HOME".into(), scratch.path().into()),
        ]);
        if sandboxed {
            let mut sandbox = Sandbox::for_home(std::env::var_os("HOME").unwrap());
            sandbox.readable.push(common.clone());
            sandbox.writable.push(scratch.path().to_path_buf());
            native = native.sandboxed(sandbox).unwrap();
        }
        let native = Arc::new(native);
        let store = session_store(&scratch, OutputCaps::DEFAULT);
        let outputs = CallOutputs::new(store.clone(), SecretSet::new());
        let tool = wasm_tool(
            &module,
            Services {
                process: Some(Arc::new(
                    ProcessCapability::new(native).storing(outputs.clone()),
                )),
                tool_outputs: Some(Arc::new(outputs.clone())),
                ..Services::default()
            },
            ExecutionLimits::default(),
            &Arc::new(p1_redact::MaskCounter::new()),
        )
        .unwrap();
        for run in 1..=2 {
            let printed_path = scratch.path().join("printed");
            let script = format!(
                "set -o pipefail; git log -p -n 200 | tee '{}'",
                printed_path.display()
            );
            let outcome = tool
                .execute(
                    &ToolCall {
                        call_id: format!("case8-{run}"),
                        name: "shell".into(),
                        input: ToolInput::Json(
                            json!({"command": script, "raw": true, "timeout_seconds": 600})
                                .to_string(),
                        ),
                    },
                    ToolContext {
                        cancel: CancellationToken::new(),
                    },
                )
                .await;
            assert!(outcome.content.ends_with("[exit code: 0]"));
            let info = outputs.produced().pop().unwrap();
            let printed = std::fs::read(&printed_path).unwrap();
            let mut redactor = super::redact::StreamRedactor::new(SecretSet::new());
            let mut masked = String::new();
            for chunk in printed.chunks(16 * 1024) {
                masked.push_str(&redactor.push(chunk));
            }
            masked.push_str(&redactor.finish());
            let stored = stored_file(&store, &info.handle);
            println!(
                "case8 sandboxed={sandboxed} run={run} printed={} masked={} stored={} capture={:?}",
                printed.len(),
                masked.len(),
                stored.len(),
                info.capture
            );
            assert_eq!(info.stored_bytes, stored.len() as u64);
            assert_eq!(info.capture, Capture::Complete);
            assert!(
                stored == masked.as_bytes(),
                "masked stream differs from stored bytes"
            );
        }
    }
}

// --- the values that cross the boundary ----------------------------------------------------------------

#[test]
fn values_cross_by_their_wit_names() {
    assert_eq!(
        info_val(OutputInfo {
            handle: "out-1".to_owned(),
            stored_bytes: 3,
            capture: Capture::StoredCapReached,
        }),
        Val::Record(vec![
            ("handle".to_owned(), Val::String("out-1".to_owned())),
            ("stored-bytes".to_owned(), Val::U64(3)),
            (
                "capture".to_owned(),
                Val::Enum("stored-cap-reached".to_owned())
            ),
        ])
    );
    assert_eq!(
        error_val(OutputError::OffsetPastEnd(6)),
        Val::Variant("offset-past-end".to_owned(), Some(Box::new(Val::U64(6))))
    );
    assert_eq!(
        error_val(OutputError::UnknownOutput),
        Val::Variant("unknown-output".to_owned(), None)
    );
    assert_eq!(
        page_val(OutputPage {
            text: "t".to_owned(),
            next_offset: 1,
            at_end: true,
        }),
        Val::Record(vec![
            ("text".to_owned(), Val::String("t".to_owned())),
            ("next-offset".to_owned(), Val::U64(1)),
            ("at-end".to_owned(), Val::Bool(true)),
        ])
    );
}
