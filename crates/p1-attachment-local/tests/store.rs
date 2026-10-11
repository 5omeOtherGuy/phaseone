//! The local store on scratch directories: round trip, content addressing, the pinned
//! directory rules, the re-verified read and garbage collection. No real home directory is
//! touched.

use std::collections::HashSet;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use p1_attachment_local::LocalAttachmentStore;
use p1_contracts::{AttachmentError, AttachmentStore, ImageTarget, ImageType};

/// A 2x3 PNG with a text chunk (zero CRCs: the store cuts chunks, it never checks CRCs).
fn png_with_text() -> Vec<u8> {
    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&[0; 4]);
        out
    }
    let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
    out.extend(chunk(b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 3, 8, 6, 0, 0, 0]));
    out.extend(chunk(b"tEXt", b"Author\0someone"));
    out.extend(chunk(b"IDAT", &[1, 2, 3]));
    out.extend(chunk(b"IEND", &[]));
    out
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn an_image_round_trips_stripped_content_addressed_and_read_only() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("state/attachments");
    let store = LocalAttachmentStore::new(&root);
    assert!(!root.exists(), "a new store creates nothing");

    let image = store.save_image(png_with_text(), None).await.unwrap();
    assert_eq!(image.media_type, ImageType::Png);
    assert_eq!((image.width, image.height), (2, 3));
    let bytes = store.read_image(&image, None).await.unwrap();
    assert_eq!(bytes.len() as u64, image.bytes);
    assert!(
        !String::from_utf8_lossy(&bytes).contains("someone"),
        "metadata is cut"
    );

    let object = root.join("objects").join(image.id.as_str());
    assert_eq!(mode(&object), 0o400);
    assert_eq!(mode(&root), 0o700);
    assert_eq!(mode(&root.join("objects")), 0o700);

    // The same content is the same object; nothing is staged or left behind.
    let again = store.save_image(png_with_text(), None).await.unwrap();
    assert_eq!(again, image);
    let names: Vec<_> = std::fs::read_dir(root.join("objects")).unwrap().collect();
    assert_eq!(names.len(), 1);
}

#[tokio::test]
async fn a_file_is_kept_verbatim_at_a_readable_path() {
    let scratch = tempfile::tempdir().unwrap();
    let store = LocalAttachmentStore::new(scratch.path().join("attachments"));
    let file = store
        .save_file(b"line one\nline two\n".to_vec(), "dir/notes.txt".into())
        .await
        .unwrap();
    assert_eq!(file.name, "notes.txt");
    assert_eq!(file.bytes, 18);
    let path = store.file_path(&file);
    assert_eq!(std::fs::read(&path).unwrap(), b"line one\nline two\n");
    assert_eq!(mode(&path), 0o400);
}

#[tokio::test]
async fn a_changed_object_is_refused_on_read() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("attachments");
    let store = LocalAttachmentStore::new(&root);
    let image = store.save_image(png_with_text(), None).await.unwrap();
    let object = root.join("objects").join(image.id.as_str());
    let mut bytes = std::fs::read(&object).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    std::fs::set_permissions(&object, std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::write(&object, &bytes).unwrap();
    assert_eq!(
        store.read_image(&image, None).await,
        Err(AttachmentError::Corrupt(image.id.clone()))
    );
    std::fs::remove_file(&object).unwrap();
    assert_eq!(
        store.read_image(&image, None).await,
        Err(AttachmentError::NotFound(image.id.clone()))
    );
}

#[tokio::test]
async fn an_image_above_the_route_target_is_refused_not_shrunk() {
    let scratch = tempfile::tempdir().unwrap();
    let store = LocalAttachmentStore::new(scratch.path().join("attachments"));
    let image = store.save_image(png_with_text(), None).await.unwrap();
    let target = ImageTarget {
        max_bytes: 1_000_000,
        max_dimension: 2,
    };
    assert!(matches!(
        store.read_image(&image, Some(target)).await,
        Err(AttachmentError::TooManyPixels { limit: 2, .. })
    ));
}

#[tokio::test]
async fn a_symlinked_or_shared_root_is_refused() {
    let scratch = tempfile::tempdir().unwrap();
    let real = scratch.path().join("real");
    std::fs::create_dir(&real).unwrap();
    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o700)).unwrap();
    let link = scratch.path().join("link");
    std::os::unix::fs::symlink(&real, &link).unwrap();
    let error = LocalAttachmentStore::new(&link)
        .save_file(b"x".to_vec(), "x".into())
        .await
        .unwrap_err();
    assert!(matches!(error, AttachmentError::Io(_)), "{error}");

    std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o777)).unwrap();
    let error = LocalAttachmentStore::new(&real)
        .save_file(b"x".to_vec(), "x".into())
        .await
        .unwrap_err();
    assert!(
        error.to_string().contains("writable by other users"),
        "{error}"
    );
}

#[tokio::test]
async fn a_non_image_is_refused_before_anything_is_stored() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("attachments");
    let store = LocalAttachmentStore::new(&root);
    assert_eq!(
        store.save_image(b"%PDF-1.7".to_vec(), None).await,
        Err(AttachmentError::UnsupportedImage)
    );
    assert!(!root.exists());
}

/// Set an object's modification time (its owner may, though it is 0400).
fn age(path: &Path, modified: SystemTime) {
    std::fs::File::open(path)
        .unwrap()
        .set_modified(modified)
        .unwrap();
}

#[tokio::test]
async fn collection_deletes_old_unreferenced_objects_only() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("attachments");
    let objects = root.join("objects");
    let store = LocalAttachmentStore::new(&root);
    let kept = store.save_file(b"kept".to_vec(), "k".into()).await.unwrap();
    let old = store.save_file(b"old".to_vec(), "o".into()).await.unwrap();
    let young = store
        .save_file(b"young".to_vec(), "y".into())
        .await
        .unwrap();
    let now = SystemTime::now();
    let cutoff = now - Duration::from_secs(30 * 86_400);
    let long_ago = now - Duration::from_secs(90 * 86_400);
    age(&objects.join(kept.id.as_str()), long_ago);
    age(&objects.join(old.id.as_str()), long_ago);
    // Not ours to collect, however old: a staging leftover and a symlink named like an
    // object (its own time and its target's).
    let outside = scratch.path().join("outside");
    std::fs::write(&outside, "not the store's").unwrap();
    let fake = "f".repeat(64);
    std::os::unix::fs::symlink(&outside, objects.join(&fake)).unwrap();
    std::fs::write(objects.join(".staging-1-2-3"), "partial").unwrap();
    age(&outside, long_ago);
    age(&objects.join(".staging-1-2-3"), long_ago);
    let secs = long_ago.duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
    let then = rustix::fs::Timespec {
        tv_sec: secs,
        tv_nsec: 0,
    };
    rustix::fs::utimensat(
        rustix::fs::CWD,
        objects.join(&fake),
        &rustix::fs::Timestamps {
            last_access: then,
            last_modification: then,
        },
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )
    .unwrap();
    let referenced = HashSet::from([kept.id.clone()]);

    let would = store.collect(&referenced, cutoff, false).unwrap();
    assert_eq!(
        would.iter().map(|o| (&o.id, o.bytes)).collect::<Vec<_>>(),
        [(&old.id, 3)]
    );
    assert!(
        objects.join(old.id.as_str()).exists(),
        "a dry run deletes nothing"
    );

    let deleted = store.collect(&referenced, cutoff, true).unwrap();
    assert_eq!(deleted, would);
    let mut left: Vec<String> = std::fs::read_dir(&objects)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    left.sort();
    let mut expected = vec![
        ".staging-1-2-3".to_string(),
        kept.id.to_string(),
        young.id.to_string(),
        fake,
    ];
    expected.sort();
    assert_eq!(left, expected);
    assert_eq!(std::fs::read(&outside).unwrap(), b"not the store's");
    assert!(store.collect(&referenced, cutoff, true).unwrap().is_empty());

    // A store that never saved anything has nothing to collect.
    let empty = LocalAttachmentStore::new(scratch.path().join("none"));
    assert!(
        empty
            .collect(&HashSet::new(), now, true)
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn saving_an_object_again_makes_it_young() {
    let scratch = tempfile::tempdir().unwrap();
    let root = scratch.path().join("attachments");
    let store = LocalAttachmentStore::new(&root);
    let file = store
        .save_file(b"again".to_vec(), "a".into())
        .await
        .unwrap();
    let now = SystemTime::now();
    age(
        &root.join("objects").join(file.id.as_str()),
        now - Duration::from_secs(90 * 86_400),
    );
    store
        .save_file(b"again".to_vec(), "a".into())
        .await
        .unwrap();
    let cutoff = now - Duration::from_secs(86_400);
    assert!(
        store
            .collect(&HashSet::new(), cutoff, true)
            .unwrap()
            .is_empty()
    );
    assert!(store.file_path(&file).exists());
}
