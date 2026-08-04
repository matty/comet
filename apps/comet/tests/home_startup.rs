#![cfg(windows)]

use std::process::Command;

#[test]
fn starts_without_home_using_the_windows_profile_directory() {
    let profile = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_comet"))
        .arg("status")
        .env_remove("HOME")
        .env("USERPROFILE", profile.path())
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "status failed:\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let expected = profile.path().join(".comet-native");
    assert!(
        stdout.contains(&format!("Data dir: {}", expected.display())),
        "unexpected stdout:\n{stdout}"
    );
}
