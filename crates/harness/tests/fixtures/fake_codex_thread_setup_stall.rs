//! Fake Codex app-server that answers `initialize` but never answers
//! `thread/start`/`thread/resume` — D45's "thread setup never answers",
//! distinct from `fake_codex_init_stall.rs`'s "initialize never answers".
//!
//! `run_session`'s `setup` future (`crates/harness/src/codex/mod.rs`) wraps
//! BOTH the `initialize` round trip and the `thread/start`-or-`thread/resume`
//! round trip in one `tokio::time::timeout(startup_timeout, setup)`. Every
//! existing startup-timeout test only ever stalled on the FIRST await inside
//! that block; this fixture answers it and stalls on the second, so a
//! regression that moved the timeout to wrap only `initialize` (or dropped it
//! around the thread call entirely) would still pass every other test in this
//! suite.
//!
//! A native Rust fixture is required here, like its sibling stall/crash
//! fixtures: Windows `CreateProcess` cannot launch the shell-script fixtures
//! commonly used for child lifecycle tests.

use std::io::{BufRead, Write};
use std::time::Duration;

/// The request id: the last `"id":<digits>` on the line, mirroring
/// `fake_codex.rs`'s own `rid` — duplicated rather than shared because this
/// file is a separate `[[bin]]` target with no `mod` to import it from.
fn rid(line: &str) -> String {
    match line.rfind("\"id\":") {
        Some(at) => line[at + 5..]
            .chars()
            .take_while(char::is_ascii_digit)
            .collect(),
        None => String::new(),
    }
}

fn main() {
    let stdin = std::io::stdin();
    let mut stdin = stdin.lock();

    // ---- initialize: answered for real ------------------------------------
    let mut init_line = String::new();
    let _ = stdin.read_line(&mut init_line);
    let init_line = init_line.trim_end_matches(['\r', '\n']);
    {
        let mut out = std::io::stdout();
        let _ = writeln!(
            out,
            r#"{{"id":{},"result":{{"userAgent":"fake-codex"}}}}"#,
            rid(init_line)
        );
        let _ = out.flush();
    }

    // ---- initialized notification: no reply expected -----------------------
    let mut initialized_line = String::new();
    let _ = stdin.read_line(&mut initialized_line);

    // ---- thread/start or thread/resume: read, then never answer -----------
    let mut thread_line = String::new();
    let _ = stdin.read_line(&mut thread_line);

    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "D45_THREAD_SETUP_STALL_PRIVATE_DIAGNOSTIC");
    let _ = stderr.flush();
    drop(stderr);

    std::thread::sleep(Duration::from_secs(30));
}
