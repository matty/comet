//! Fake Codex app-server that exits while initialize is in flight.
//!
//! The poison diagnostic proves provider stderr stays owner-local rather than
//! being copied into the normalized transcript event.

use std::io::{BufRead, Write};

fn main() {
    let mut line = String::new();
    let _ = std::io::stdin().lock().read_line(&mut line);

    let mut stderr = std::io::stderr().lock();
    let _ = writeln!(stderr, "TASK81_INIT_CRASH_PRIVATE_DIAGNOSTIC");
    let _ = stderr.flush();
    std::process::exit(73);
}
