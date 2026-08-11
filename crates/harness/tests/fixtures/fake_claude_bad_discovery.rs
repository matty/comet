//! Fake Claude CLI that answers the discovery handshake with a frame comet
//! cannot read — the `Unparseable` half of `DiscoveryFailure`, which is the
//! only failure that raises a protocol-drift `Diagnostic`.
//!
//! A separate binary rather than a scenario flag on `fake_claude`: a discovery
//! session sends no prompt to carry a scenario marker, and the adapter passes
//! the child no env of its own.

use std::io::{BufRead, Write};

fn main() {
    let stdin = std::io::stdin();
    let mut line = String::new();
    if stdin.lock().read_line(&mut line).unwrap_or(0) == 0 {
        return;
    }
    let mut out = std::io::stdout();
    // Right frame type, wrong shape underneath: `models` is a string.
    let _ = writeln!(
        out,
        r#"{{"type":"control_response","response":{{"subtype":"success","request_id":"comet-discovery-1","response":{{"models":"all of them"}}}}}}"#
    );
    let _ = out.flush();
}
