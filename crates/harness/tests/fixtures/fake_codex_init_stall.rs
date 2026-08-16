//! Fake Codex app-server that accepts initialize but never answers it.
//!
//! A native Rust fixture is required here: Windows `CreateProcess` cannot
//! launch the shell-script fixtures commonly used for child lifecycle tests.

use std::io::{BufRead, Write};
use std::time::Duration;

fn main() {
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);

    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "TASK81_INIT_STALL_PRIVATE_DIAGNOSTIC");
    let _ = stderr.flush();
    drop(stderr);

    std::thread::sleep(Duration::from_secs(30));
}
