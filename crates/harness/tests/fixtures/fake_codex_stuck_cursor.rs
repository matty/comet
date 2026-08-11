//! Fake Codex app-server whose `model/list` always answers with the same
//! `nextCursor`.
//!
//! The cursor is opaque and server-chosen, so nothing in the schema stops a
//! server from handing back a cursor that never advances. A client that pages
//! until the cursor is null would then never stop — against a picker that is
//! awaiting the answer, with a slow-request toast already on screen.
//!
//! **It stops answering after three pages**, deliberately: the client's page
//! cap would otherwise end the loop too, and a test that cannot tell the two
//! guards apart passes with either one deleted. Closing stdout early makes the
//! difference visible — the cursor guard reports drift, while falling through
//! to EOF reports an unreachable CLI.

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
        r#"{{"id":{},"result":{{"userAgent":"fake-codex-stuck"}}}}"#,
        rid(&init)
    ));
    let Some(_initialized) = read_line(&mut stdin) else {
        return;
    };

    let mut served = 0;
    while let Some(line) = read_line(&mut stdin) {
        if !line.contains(r#""method":"model/list""#) {
            return;
        }
        served += 1;
        if served > 3 {
            return;
        }
        emit(&format!(
            r#"{{"id":{},"result":{{"data":[{{"id":"gpt-stuck","model":"gpt-stuck","displayName":"Stuck","description":"d","hidden":false,"isDefault":false,"defaultReasoningEffort":"medium","supportedReasoningEfforts":[{{"reasoningEffort":"low","description":"d"}}],"inputModalities":["text"],"serviceTiers":[],"additionalSpeedTiers":[]}}],"nextCursor":"same"}}}}"#,
            rid(&line)
        ));
    }
}
