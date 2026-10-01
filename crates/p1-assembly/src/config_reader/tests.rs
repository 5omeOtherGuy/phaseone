use std::path::Path;

use super::*;

const INPUTS: &[&str] = &[
    "environments/test/environment.toml",
    "environments/test/prompt.md",
    "environments/test/summarize.md",
    "descriptions/read.md",
    "profiles/model.toml",
    "modules.lock",
];
const MARKER: &str = "private-config-marker";

fn setup(root: &Path) {
    for directory in ["environments/test", "descriptions", "profiles"] {
        std::fs::create_dir_all(root.join(directory)).unwrap();
    }
    std::fs::write(
        root.join(INPUTS[0]),
        format!(
            "route = 'test'\nprofile = 'model'\n[[tools]]\nmodule = 'read'\n\
             description_file = '{}'\n",
            root.join(INPUTS[3]).display()
        ),
    )
    .unwrap();
    std::fs::write(root.join(INPUTS[1]), "prompt").unwrap();
    std::fs::write(root.join(INPUTS[2]), "summary").unwrap();
    std::fs::write(root.join(INPUTS[3]), "description").unwrap();
    std::fs::write(
        root.join(INPUTS[4]),
        "id = 'model'\nrevision = 1\nmodel_id = 'model'\nfamily = 'test'\n\
         thinking = 'enabled'\nefforts = ['high']\ndefault_effort = 'high'\n\
         max_output_tokens = 4096\n",
    )
    .unwrap();
    std::fs::write(root.join(INPUTS[5]), "format = 'p1-modules-lock/1'\n").unwrap();
}

fn load(root: &Path, input: &str, reader: &ConfigReader) -> Result<(), String> {
    let directories = [root.join("environments")];
    if input == "modules.lock" {
        crate::modules_lock::load_modules_lock_with_reader(&directories, reader)
            .map(|_| ())
            .map_err(|error| error.to_string())
    } else {
        crate::load_environment_with_reader("test", &directories, reader)
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

#[test]
fn regression_every_input_refuses_symlinks_and_hard_links_to_credentials() {
    for input in INPUTS {
        for hard_link in [false, true] {
            let root = tempfile::tempdir().unwrap();
            setup(root.path());
            let home = root.path().join("home");
            let credential = home.join(".codex/auth.json");
            std::fs::create_dir_all(credential.parent().unwrap()).unwrap();
            std::fs::write(&credential, MARKER).unwrap();
            let reader = ConfigReader {
                policy: CredentialPolicy::new(Some(&home), &[]),
            };
            load(root.path(), input, &reader).unwrap();
            let path = root.path().join(input);
            std::fs::remove_file(&path).unwrap();
            if hard_link {
                std::fs::hard_link(&credential, &path).unwrap();
            } else {
                std::os::unix::fs::symlink(&credential, &path).unwrap();
            }
            let error = load(root.path(), input, &reader).expect_err(input);
            assert!(error.contains("credential"), "{input}: {error}");
            assert!(!error.contains(MARKER), "{input} leaked contents");
        }
    }
}

#[test]
fn regression_every_input_refuses_oversized_sparse_files() {
    for input in INPUTS {
        let root = tempfile::tempdir().unwrap();
        setup(root.path());
        let reader = ConfigReader {
            policy: CredentialPolicy::new(None, &[]),
        };
        load(root.path(), input, &reader).unwrap();
        std::fs::File::create(root.path().join(input))
            .unwrap()
            .set_len(MAX_CONFIG_BYTES as u64 + 1)
            .unwrap();
        let error = load(root.path(), input, &reader).expect_err(input);
        assert!(
            error.contains("byte limit"),
            "{input}: missing byte-limit refusal"
        );
    }
}

#[test]
fn every_input_refuses_fifos_without_a_writer() {
    for input in INPUTS {
        let root = tempfile::tempdir().unwrap();
        setup(root.path());
        let path = root.path().join(input);
        std::fs::remove_file(&path).unwrap();
        assert!(
            std::process::Command::new("mkfifo")
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let reader = ConfigReader {
            policy: CredentialPolicy::new(None, &[]),
        };
        assert!(load(root.path(), input, &reader).is_err(), "{input}");
    }
}

#[test]
fn ordinary_symlinks_and_hard_links_remain_valid() {
    let root = tempfile::tempdir().unwrap();
    setup(root.path());
    let reader = ConfigReader {
        policy: CredentialPolicy::new(None, &[]),
    };
    let target = root.path().join("ordinary");
    std::fs::write(&target, "ordinary prompt").unwrap();
    let prompt = root.path().join(INPUTS[1]);
    std::fs::remove_file(&prompt).unwrap();
    std::os::unix::fs::symlink(&target, &prompt).unwrap();
    load(root.path(), INPUTS[1], &reader).unwrap();
    std::fs::remove_file(&prompt).unwrap();
    std::fs::hard_link(&target, &prompt).unwrap();
    load(root.path(), INPUTS[1], &reader).unwrap();
}

#[test]
fn regression_growth_after_metadata_check_is_bounded() {
    struct GrowOnRead {
        file: std::fs::File,
        writer: std::fs::File,
    }
    impl std::io::Read for GrowOnRead {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            // Grow only after the caller checked the opened file's original length.
            self.writer.set_len(MAX_CONFIG_BYTES as u64 + 1)?;
            self.file.read(buffer)
        }
    }
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("growing");
    let writer = std::fs::File::create(&path).unwrap();
    let file = std::fs::File::open(&path).unwrap();
    assert_eq!(file.metadata().unwrap().len(), 0);
    let Err(error) = read_bounded(GrowOnRead { file, writer }) else {
        panic!("grew beyond byte limit without refusal");
    };
    assert!(error.to_string().contains("byte limit"));
}

#[test]
fn byte_limit_is_inclusive_and_invalid_utf8_is_refused() {
    let text = "x".repeat(MAX_CONFIG_BYTES);
    assert_eq!(read_bounded(text.as_bytes()).unwrap(), text);
    let error = read_bounded(&[0xff][..]).unwrap_err();
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
}

#[test]
fn lexical_and_xdg_credential_paths_are_refused() {
    let root = tempfile::tempdir().unwrap();
    let keys = root.path().join("keys");
    std::fs::create_dir(&keys).unwrap();
    let credential = keys.join("test.key");
    std::fs::write(&credential, MARKER).unwrap();
    let reader = ConfigReader {
        policy: CredentialPolicy::new(None, &[keys]),
    };
    let alias = root.path().join("alias");
    std::fs::hard_link(&credential, &alias).unwrap();
    for path in [&credential, &alias] {
        let error = reader.read(path).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(!error.to_string().contains(MARKER));
    }
    let path = root.path().join(".credentials.json");
    let ordinary = root.path().join("public");
    std::fs::write(&ordinary, "public").unwrap();
    std::os::unix::fs::symlink(&ordinary, &path).unwrap();
    assert_eq!(
        reader.read(&path).unwrap_err().kind(),
        std::io::ErrorKind::PermissionDenied
    );
}
