//! Fake Codex app-server that answers `model/list` without a `data` key — the
//! `Unparseable` half of `DiscoveryFailure`, and the shape slice 2.2 paid for
//! on the Claude side (`94fa0d3`): an absent list decoded as an empty one,
//! serving the curated catalog as if it were live.
//!
//! A separate binary rather than a scenario flag on `fake_codex`: a discovery
//! session sends no prompt to carry a marker, and the adapter passes the child
//! no env of its own.

use std::io::{BufRead, StdinLock, Write};

fn emit(line: &str) {
    let mut out = std::io::stdout();
    let _ = writeln!(out, "{line}");
    let _ = out.flush();
}

fn read_line(stdin: &mut StdinLock<'_>) -> Option<String> {
    let mut buf = String::new();
    match stdin.read_line(&mut buf) {
        Ok(0) | Err(_) => None,
        Ok(_) => Some(buf.trim_end_matches(['\r', '\n']).to_string()),
    }
}

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

    let Some(init) = read_line(&mut stdin) else {
        return;
    };
    emit(&format!(
        r#"{{"id":{},"result":{{"userAgent":"fake-codex-bad"}}}}"#,
        rid(&init)
    ));
    let Some(_initialized) = read_line(&mut stdin) else {
        return;
    };
    let Some(list) = read_line(&mut stdin) else {
        return;
    };
    // A well-formed JSON-RPC result with the model list missing entirely.
    emit(&format!(
        r#"{{"id":{},"result":{{"nextCursor":null}}}}"#,
        rid(&list)
    ));
}
