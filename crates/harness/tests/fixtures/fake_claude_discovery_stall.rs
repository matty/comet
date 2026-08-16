//! Fake Claude CLI for recorder tests that need a discovery handshake to
//! genuinely NOT complete, rather than the discovery reply `fake_claude.rs`
//! always sends. A separate binary for the same reason
//! `fake_claude_bad_discovery.rs` is: a discovery session sends no prompt to
//! carry a scenario marker, so there is no stdin content to select behavior
//! by. The one signal discovery invocations do differ on is `--bare` (model
//! discovery vs command discovery), so this fixture uses that instead:
//!
//! - `--bare` (model discovery): hangs forever after receiving the
//!   initialize request — exercises the recorder's hard-timeout kill/reap
//!   path against a genuinely live, unresponsive child.
//! - no `--bare` (command discovery): exits immediately without answering —
//!   exercises the driving-failure (`DriverError`) path deterministically
//!   and fast, without waiting out a timeout.
//!
//! Rust rather than a shell script for the same portability reason
//! `fake_claude.rs` gives: Windows cannot spawn a shell script directly.

use std::io::BufRead;

fn main() {
    let stdin = std::io::stdin();
    let mut line = String::new();
    // Read (and discard) the initialize request so a caller's write does not
    // block on a full pipe buffer, then simply never answer it.
    let _ = stdin.lock().read_line(&mut line);

    if std::env::args().any(|arg| arg == "--bare") {
        std::thread::sleep(std::time::Duration::from_secs(300));
    }
    // Non-bare: fall through and exit(0) without ever writing a reply.
}
