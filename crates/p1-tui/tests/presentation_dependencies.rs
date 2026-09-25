use std::process::Command;

#[test]
fn resolved_presentation_dependencies_stay_presentation_only() {
    let script = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/check_dependencies.py");
    let output = Command::new("python3")
        .arg(script)
        .output()
        .expect("python3 must be available for the repository gate");

    assert!(
        output.status.success(),
        "python3 {} failed with {}\nstdout:\n{}\nstderr:\n{}",
        script,
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
}
