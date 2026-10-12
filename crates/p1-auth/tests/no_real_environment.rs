//! Must-pass (f) of the spec: no test in this crate reads the real home or the
//! real process environment.
//!
//! The test proves it for the SUITE (a source scan); the connectors' suites prove it for the
//! code under test with their logins installed (`connectors/subscriptions/logins/tests`).

mod support;

use std::ffi::OsStr;
use std::path::PathBuf;

/// Every test source of this crate: the test files and the shared module.
fn test_sources() -> Vec<PathBuf> {
    let tests = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&tests)
        .expect("the tests directory")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("rs")))
        .collect();
    paths.push(tests.join("support/mod.rs"));
    paths.sort();
    paths
}

#[test]
fn no_test_reads_the_real_home_or_the_process_environment() {
    // Assembled from pieces so this file does not match itself.
    let forbidden = [
        format!("std::env::{}", "var"),
        format!("Locations::{}", "from_process"),
        format!("{}::home_dir", "dirs"),
    ];
    let sources = test_sources();
    assert!(
        sources.len() >= 8,
        "the scan must see the whole suite, saw {} files",
        sources.len()
    );
    for path in sources {
        let text = std::fs::read_to_string(&path).expect("a test source is readable");
        for pattern in &forbidden {
            assert!(
                !text.contains(pattern.as_str()),
                "{} reads the real environment or home ({pattern})",
                path.display()
            );
        }
    }
}
