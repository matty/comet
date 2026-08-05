//! End-to-end (Windows): a CLI the process PATH can't see still resolves.
//!
//! A GUI launch inherits Explorer's PATH snapshot, which goes stale the moment
//! an installer edits the persisted environment — the exact shape that made
//! Codex "not found" in the app while `codex` worked in a console. Unix
//! recovers through the login-shell snapshot (`shell_env_resolution.rs`);
//! these are the Windows equivalents: `.cmd`/`.bat` shims (npm installs) and
//! the per-user install dirs the resolver must consult as a last resort.
//!
//! This file must stay a single test: it mutates process env (PATH, HOME,
//! LOCALAPPDATA, APPDATA) and warms the process-global system-PATH cache, so
//! it needs its own test binary with no parallel siblings.

#![cfg(windows)]

use std::path::Path;

use comet_harness::{ClaudeHarness, CodexHarness, Harness};

fn touch(path: &Path) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(path, "@echo off\r\n").unwrap();
}

#[tokio::test]
async fn windows_shims_and_install_dirs_resolve() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let local = dir.path().join("local");
    let roaming = dir.path().join("roaming");
    let on_path = dir.path().join("on-path");

    // npm's global bin only ever holds `.cmd`/`.ps1` shims on Windows — never
    // a bare `codex` or `codex.exe`.
    touch(&on_path.join("codex.cmd"));
    touch(&on_path.join("claude.cmd"));

    // SAFETY: single-test binary — nothing else reads env concurrently.
    unsafe {
        std::env::set_var("PATH", on_path.as_os_str());
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        std::env::set_var("LOCALAPPDATA", &local);
        std::env::set_var("APPDATA", &roaming);
        std::env::remove_var("CODEX_EXECUTABLE");
        std::env::remove_var("CLAUDE_CODE_EXECUTABLE");
        // Keep the real machine's persisted PATH (and its real codex install)
        // out of this test: resolution must succeed on the fixtures alone.
        std::env::set_var("COMET_NO_LOGIN_SHELL", "1");
    }

    CodexHarness::new()
        .models()
        .await
        .expect("codex resolves through a .cmd shim on PATH");
    ClaudeHarness::new()
        .models()
        .await
        .expect("claude resolves through a .cmd shim on PATH");

    // Now hide the shims: only the per-user install dirs remain. This is the
    // stale-Explorer-PATH case — the app's PATH never names the install dir.
    unsafe {
        std::env::set_var("PATH", "C:\\Windows\\system32");
    }
    assert!(
        CodexHarness::new().models().await.is_err(),
        "nothing should resolve before the install-dir fixtures exist"
    );

    // The official Codex Windows installer's per-user location.
    touch(&local.join("Programs\\OpenAI\\Codex\\bin\\codex.exe"));
    // The Claude Code native installer's per-user location.
    touch(&home.join(".local\\bin\\claude.exe"));

    CodexHarness::new()
        .models()
        .await
        .expect("codex resolves from %LOCALAPPDATA%\\Programs\\OpenAI\\Codex\\bin");
    ClaudeHarness::new()
        .models()
        .await
        .expect("claude resolves from %USERPROFILE%\\.local\\bin");

    // Discovery is worthless if the shim can't then be spawned: a `.cmd` is
    // not a PE image, so it only runs because std routes batch files through
    // cmd.exe. Prove the harness's own spawn shape (piped stdio) works on one.
    let shim = dir.path().join("speak.cmd");
    std::fs::write(&shim, "@echo off\r\necho hello-from-shim\r\n").unwrap();
    let out = tokio::process::Command::new(&shim)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .expect("a .cmd shim spawns");
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        "hello-from-shim"
    );
}
